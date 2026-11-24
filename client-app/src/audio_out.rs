// SPDX-License-Identifier: LGPL-3.0-or-later
//! Audio output stage: [`AudioSink`] backends (PipeWire/PulseAudio/ALSA
//! subprocess pipe, null, WAV file), the bounded PCM queue that feeds a
//! stalled sink without unbounded growth, and [`AudioRuntime`] — the live
//! pipeline that turns decoded `sound_*` packets into spatialized, limited
//! stereo PCM.
//!
//! The runtime lives inside [`crate::protocol::ProtocolClient`]: it is fed
//! by the play-packet decode arms and pumped from the same call sites the
//! session loop already drives, so it needs no separate audio thread for
//! mixing. Wall-clock reads exist only in [`AudioRuntime::pump`], which
//! paces how many samples are *due*; the mixer itself
//! ([`crate::session::SoundMixer`]) is deterministic for fixed inputs.

use std::cell::UnsafeCell;
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use megu_client_mesh::tactile_effects::EnvironmentAudioHint;

use crate::audio_qol::AudioLimiter;
use crate::protocol::{WireSoundEvent, WireStopSound};
use crate::session::{AudioStats, OpenAirWorld, PlaySoundEvent, SoundMixer};
use crate::sound_assets::{
    normalize_sound_name, placeholder_pcm, sound_name_for_id, DecodedPcm, SoundEventRegistry,
    SoundLibrary, SoundSourceData, MIX_SAMPLE_RATE,
};
use crate::spatial_audio::{
    ListenerPose, SharedListenerFdnBus, SoundSource, SpatialAudioEngine, SpatialAudioPreset,
};

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
    /// Whether missed mix deadlines can starve a time-sensitive output.
    /// Buffered native audio backends use the default; sinks that discard or
    /// persist samples can opt out of device-underrun accounting.
    fn requires_realtime_cadence(&self) -> bool {
        true
    }
    /// Finalize output (patch WAV headers, close the child stdin and reap).
    /// Idempotent; also invoked from `Drop`.
    fn finish(&mut self) {}
}

/// Selected backend from `--audio-sink`/`--audio-wav`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioSinkKind {
    /// Native backend in OS probe order (`pw-cat`/`paplay`/`aplay` on Linux,
    /// CoreAudio on macOS, WinMM on Windows), else `null` (with a warning).
    Auto,
    /// Force `pw-cat`; falls back to `null` with a warning when it cannot
    /// start or dies.
    PipeWire,
    /// Force `paplay` (PulseAudio or PipeWire's PulseAudio-compat layer);
    /// same null fallback. Not exposed via `--audio-sink`.
    PulseAudio,
    /// Force `aplay`; same null fallback.
    Alsa,
    /// Force the macOS CoreAudio/AudioQueue backend; null fallback when the
    /// queue cannot start (headless or no output device).
    CoreAudio,
    /// Force the Windows `winmm.dll` `waveOut*` backend; null fallback when
    /// the device cannot open (headless or no output device).
    WinMm,
    /// Discard samples (still counts them) — the fail-closed default.
    Null,
    /// Stream a 48 kHz s16le stereo WAV to the given path (`--audio-wav`).
    Wav(PathBuf),
    /// No runtime at all — audio disabled.
    Off,
}

/// Operating system a sink probe runs on — explicit parameter so the
/// probe ordering is unit-testable without cross-compiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetOs {
    /// Linux (and other Unixes that ship `pw-cat`/`paplay`/`aplay`).
    Linux,
    /// macOS — CoreAudio/AudioQueue.
    MacOs,
    /// Windows — `winmm.dll` `waveOut*`.
    Windows,
    /// Anything else — only the null sink exists.
    Other,
}

/// The OS this build targets.
pub fn host_os() -> TargetOs {
    if cfg!(target_os = "macos") {
        TargetOs::MacOs
    } else if cfg!(target_os = "windows") {
        TargetOs::Windows
    } else if cfg!(target_os = "linux") || cfg!(unix) {
        TargetOs::Linux
    } else {
        TargetOs::Other
    }
}

/// Sorted immediate children of `dir` matching `kind`: a file extension
/// (`"jar"`, `"zip"`) or `"dir"` for directories. Missing/unreadable dirs
/// yield an empty list — the sound-layer scan treats them as absent.
fn sorted_dir_entries(dir: &Path, kind: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let matches = if kind == "dir" {
            path.is_dir()
        } else {
            path.is_file()
                && path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case(kind))
        };
        if matches {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// `true` when `path` names an executable file (no spawn — pure stat).
fn path_has_executable(prog: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(prog);
        if cand.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if cand
                    .metadata()
                    .map(|m| m.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false)
                {
                    return true;
                }
            }
            #[cfg(not(unix))]
            {
                return true;
            }
        }
    }
    false
}

/// Availability probe for the *native* backend `key` (`"pipewire"`,
/// `"pulseaudio"`, `"alsa"`, `"coreaudio"`, `"winmm"`). Injected into
/// [`detect_available_sinks_for_os`] in production; tests substitute a stub
/// so probe order is checked without spawning or cross-compiling.
///
/// Framework backends (CoreAudio/WinMM) report availability of the
/// *framework* — the queue/device open inside `open_sink` is what decides
/// whether real output exists, and it fails closed to [`NullSink`].
pub fn native_backend_available(key: &str) -> bool {
    match key {
        "pipewire" => path_has_executable("pw-cat"),
        "pulseaudio" => path_has_executable("paplay"),
        "alsa" => path_has_executable("aplay"),
        "coreaudio" => cfg!(target_os = "macos"),
        "winmm" => cfg!(target_os = "windows"),
        _ => false,
    }
}

/// Ordered native sink candidates for `os`, filtered by `available`.
///
/// - Linux → `[PipeWire, PulseAudio, Alsa]` (PipeWire first — the modern
///   stack; PulseAudio/`paplay` covers distros without `pw-cat` such as
///   Ubuntu 22.04, Debian 12, Void, Alpine, and NixOS)
/// - macOS → `[CoreAudio]`
/// - Windows → `[WinMm]`
/// - anything else → `[]`
///
/// When the probe filters out every native backend (headless, missing
/// tools) the list is `[Null]` — the fail-closed sentinel — so callers can
/// iterate the list and always end on a working sink.
pub fn detect_available_sinks_for_os(
    os: TargetOs,
    available: impl Fn(&str) -> bool,
) -> Vec<AudioSinkKind> {
    let candidates: &[(&str, AudioSinkKind)] = match os {
        TargetOs::Linux => &[
            ("pipewire", AudioSinkKind::PipeWire),
            ("pulseaudio", AudioSinkKind::PulseAudio),
            ("alsa", AudioSinkKind::Alsa),
        ],
        TargetOs::MacOs => &[("coreaudio", AudioSinkKind::CoreAudio)],
        TargetOs::Windows => &[("winmm", AudioSinkKind::WinMm)],
        TargetOs::Other => &[],
    };
    let mut kinds: Vec<AudioSinkKind> = candidates
        .iter()
        .filter(|(key, _)| available(key))
        .map(|(_, kind)| kind.clone())
        .collect();
    if kinds.is_empty() {
        kinds.push(AudioSinkKind::Null);
    }
    kinds
}

/// Serialize `pcm` (interleaved s16 stereo) to little-endian bytes — the
/// on-the-wire/on-disk format every PCM sink shares.
pub fn encode_pcm_le(pcm: &[i16]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(pcm.len() * 2);
    for s in pcm {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    bytes
}

/// Exact 44-byte RIFF/WAVE header for `data_bytes` of s16 stereo PCM at
/// `MIX_SAMPLE_RATE` — the same header [`WavSink`] writes and patches.
/// Sizes saturate at `u32::MAX` (the RIFF ceiling).
pub fn wav_header_bytes(data_bytes: u64) -> [u8; 44] {
    let data = data_bytes.min(u64::from(u32::MAX)) as u32;
    let total = 36u64
        .saturating_add(data_bytes)
        .min(u64::from(u32::MAX)) as u32;
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&total.to_le_bytes());
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
    h[40..44].copy_from_slice(&data.to_le_bytes());
    h
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
    fn requires_realtime_cadence(&self) -> bool {
        false
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
        // Sizes are zero up front and patched on finish.
        self.file
            .write_all(&wav_header_bytes(0))
            .map_err(|e| format!("{}: header write failed: {e}", self.path.display()))
    }
}

impl AudioSink for WavSink {
    fn push(&mut self, pcm: &[i16]) -> Result<(), String> {
        let bytes = encode_pcm_le(pcm);
        self.file
            .write_all(&bytes)
            .map_err(|e| format!("{}: write failed: {e}", self.path.display()))?;
        self.data_bytes += bytes.len() as u64;
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
        let _ = f.seek(SeekFrom::Start(0));
        let _ = f.write_all(&wav_header_bytes(self.data_bytes));
        let _ = f.flush();
    }
}

impl Drop for WavSink {
    fn drop(&mut self) {
        self.finish();
    }
}

// ---------------------------------------------------------------------------
// Subprocess sink (pw-cat / paplay / aplay) + bounded queue
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
    #[cfg(test)]
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

    /// Block until a chunk arrives, or return `None` once closed and drained.
    /// Unlike [`Self::pop`], idle silence never terminates the stream.
    fn pop_blocking(&self) -> Option<Vec<u8>> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(chunk) = g.chunks.pop_front() {
                g.bytes = g.bytes.saturating_sub(chunk.len());
                return Some(chunk);
            }
            if g.closed {
                return None;
            }
            g = self
                .not_empty
                .wait(g)
                .unwrap_or_else(|e| e.into_inner());
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

/// `pw-cat`/`paplay`/`aplay` subprocess sink. Mixed PCM is queued on a
/// bounded buffer and drained into the child's stdin by a writer thread; a
/// dead or stalled child fails the sink closed (the runtime swaps in
/// [`NullSink`]).
pub struct SubprocessSink {
    prog: &'static str,
    label: &'static str,
    child: Option<Child>,
    queue: Arc<PcmQueue>,
    writer_done: Arc<AtomicBool>,
    dead: Arc<AtomicBool>,
    stderr_tail: Arc<Mutex<String>>,
    writer: Option<JoinHandle<()>>,
    finished: bool,
}

impl SubprocessSink {
    fn spawn(prog: &'static str, args: &[&str], label: &'static str) -> Result<Self, String> {
        let mut child = Command::new(prog)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot start {prog}: {e}"))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("cannot open stdin for {prog}"))?;
        // Drain stderr into a bounded tail buffer: when the child dies the
        // reason it printed (e.g. `pw-cat` failing to connect to the
        // daemon) is the only diagnostic there is.
        let stderr_tail: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        if let Some(mut err) = child.stderr.take() {
            let tail = stderr_tail.clone();
            std::thread::Builder::new()
                .name("megu-audio-stderr".to_string())
                .spawn(move || {
                    let mut buf = [0u8; 1024];
                    while let Ok(n) = err.read(&mut buf) {
                        if n == 0 {
                            return;
                        }
                        if let Ok(mut t) = tail.lock() {
                            t.push_str(&String::from_utf8_lossy(&buf[..n]));
                            if t.len() > 4096 {
                                let drop = t.len() - 4096;
                                t.drain(..drop);
                            }
                        }
                    }
                })
                .ok();
        }
        let queue = Arc::new(PcmQueue::new(SINK_QUEUE_BYTES));
        let writer_done = Arc::new(AtomicBool::new(false));
        let dead = Arc::new(AtomicBool::new(false));
        let writer = {
            let queue = queue.clone();
            let writer_done = writer_done.clone();
            let dead = dead.clone();
            std::thread::Builder::new()
                .name("megu-audio-sink".to_string())
                .spawn(move || {
                    let _mark_done = writer_done;
                    while let Some(chunk) = queue.pop_blocking() {
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
            stderr_tail,
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
                Ok(Some(status)) => {
                    self.dead.store(true, Ordering::Relaxed);
                    let tail = self
                        .stderr_tail
                        .lock()
                        .map(|t| t.trim().to_string())
                        .unwrap_or_default();
                    return Err(if tail.is_empty() {
                        format!("{} exited while streaming ({status})", self.prog)
                    } else {
                        format!(
                            "{} exited while streaming ({status}): {tail}",
                            self.prog
                        )
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    self.dead.store(true, Ordering::Relaxed);
                    return Err(format!("cannot poll {}: {e}", self.prog));
                }
            }
        }
        self.queue.push(encode_pcm_le(pcm));
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

// ---------------------------------------------------------------------------
// Native platform sinks
// ---------------------------------------------------------------------------

/// PCM bytes one native buffer carries: 4096 stereo frames ≈ 85 ms.
#[cfg(any(target_os = "macos", target_os = "windows"))]
const NATIVE_BUF_BYTES: usize = 16_384;
/// Native buffers allowed in flight — ~0.5 s of audio, hard bound.
#[cfg(any(target_os = "macos", target_os = "windows"))]
const NATIVE_MAX_BUFS: usize = 6;
/// How long `push` waits for a device buffer before failing closed.
#[cfg(any(target_os = "macos", target_os = "windows"))]
const NATIVE_WAIT_TIMEOUT: Duration = Duration::from_millis(500);

/// Open the macOS CoreAudio/AudioQueue sink. Fails (→ null fallback) when
/// the queue cannot be created or started — no device, headless session.
#[cfg(target_os = "macos")]
fn coreaudio_sink() -> Result<Box<dyn AudioSink>, String> {
    coreaudio::CoreAudioSink::open().map(|s| Box::new(s) as Box<dyn AudioSink>)
}

/// Non-macOS stub: the backend does not exist here.
#[cfg(not(target_os = "macos"))]
fn coreaudio_sink() -> Result<Box<dyn AudioSink>, String> {
    Err("coreaudio backend is only available on macOS".to_string())
}

/// Open the Windows WinMM `waveOut*` sink. Fails (→ null fallback) when no
/// output device can be opened — headless session, driver missing.
#[cfg(target_os = "windows")]
fn winmm_sink() -> Result<Box<dyn AudioSink>, String> {
    winmm::WinMmSink::open().map(|s| Box::new(s) as Box<dyn AudioSink>)
}

/// Non-Windows stub: the backend does not exist here.
#[cfg(not(target_os = "windows"))]
fn winmm_sink() -> Result<Box<dyn AudioSink>, String> {
    Err("winmm backend is only available on Windows".to_string())
}

/// Native macOS output through AudioToolbox `AudioQueue`.
///
/// `unsafe` boundary: every FFI declaration and call is confined to this
/// module behind `#[cfg(target_os = "macos")]`. The `AudioQueue` API is
/// documented thread-safe for enqueue/stop from any thread, and the
/// buffer-done callback only pushes a pointer back onto a mutex-guarded
/// free list, so `unsafe impl Send` is sound. The [`Pool`] the callback
/// dereferences is `Box::leak`ed for the queue's whole lifetime and
/// reclaimed in [`CoreAudioSink::finish`] after `AudioQueueDispose`.
#[cfg(target_os = "macos")]
mod coreaudio {
    use super::{AudioSink, NATIVE_BUF_BYTES, NATIVE_MAX_BUFS, NATIVE_WAIT_TIMEOUT};
    use super::{encode_pcm_le, SINK_DRAIN_TIMEOUT};
    use std::ffi::c_void;
    use std::sync::{Condvar, Mutex};
    use std::time::Instant;

    type OSStatus = i32;
    type AudioQueueRef = *mut c_void;
    type AudioQueueBufferRef = *mut AudioQueueBuffer;

    const K_AUDIO_FORMAT_LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");
    /// `kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked`.
    const PCM_FORMAT_FLAGS: u32 = (1 << 3) | (1 << 2);

    #[repr(C)]
    struct AudioStreamBasicDescription {
        sample_rate: f64,
        format_id: u32,
        format_flags: u32,
        bytes_per_packet: u32,
        frames_per_packet: u32,
        bytes_per_frame: u32,
        channels_per_frame: u32,
        bits_per_channel: u32,
        reserved: u32,
    }

    #[repr(C)]
    struct AudioQueueBuffer {
        audio_data_bytes_capacity: u32,
        audio_data: *mut c_void,
        audio_data_size: u32,
        user_data: *mut c_void,
    }

    type OutputCallback =
        extern "C" fn(*mut c_void, AudioQueueRef, AudioQueueBufferRef);

    #[link(name = "AudioToolbox", kind = "framework")]
    extern "C" {
        fn AudioQueueNewOutput(
            format: *const AudioStreamBasicDescription,
            callback: Option<OutputCallback>,
            user_data: *mut c_void,
            callback_run_loop: *mut c_void,
            callback_run_loop_mode: *mut c_void,
            flags: u32,
            out: *mut AudioQueueRef,
        ) -> OSStatus;
        fn AudioQueueAllocateBuffer(
            queue: AudioQueueRef,
            size: u32,
            out: *mut AudioQueueBufferRef,
        ) -> OSStatus;
        fn AudioQueueEnqueueBuffer(
            queue: AudioQueueRef,
            buf: AudioQueueBufferRef,
            num_packet_descs: u32,
            packet_descs: *const c_void,
        ) -> OSStatus;
        fn AudioQueueFreeBuffer(queue: AudioQueueRef, buf: AudioQueueBufferRef) -> OSStatus;
        fn AudioQueueStart(queue: AudioQueueRef, start_time: *const c_void) -> OSStatus;
        fn AudioQueueStop(queue: AudioQueueRef, immediate: bool) -> OSStatus;
        fn AudioQueueDispose(queue: AudioQueueRef, immediate: bool) -> OSStatus;
    }

    /// Free-buffer list the AudioQueue callback refills.
    struct Pool {
        free: Mutex<Vec<AudioQueueBufferRef>>,
        cond: Condvar,
        allocated: Mutex<usize>,
    }

    impl Pool {
        /// Take a free buffer, allocating one when under the cap; `None`
        /// when every buffer is in flight.
        fn take(&self, queue: AudioQueueRef) -> Option<AudioQueueBufferRef> {
            if let Some(buf) = self
                .free
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop()
            {
                return Some(buf);
            }
            let mut allocated = self.allocated.lock().unwrap_or_else(|e| e.into_inner());
            if *allocated >= NATIVE_MAX_BUFS {
                return None;
            }
            let mut buf = std::ptr::null_mut();
            // SAFETY: `queue` is a live AudioQueueRef; `buf` is a valid
            // out-pointer. The returned buffer is owned by this pool.
            let status = unsafe { AudioQueueAllocateBuffer(queue, NATIVE_BUF_BYTES as u32, &mut buf) };
            if status == 0 && !buf.is_null() {
                *allocated += 1;
                Some(buf)
            } else {
                None
            }
        }

        /// Wait for a free buffer, allocating greedily at first. `Err`
        /// when nothing frees up inside [`NATIVE_WAIT_TIMEOUT`].
        fn take_wait(&self, queue: AudioQueueRef) -> Result<AudioQueueBufferRef, String> {
            let deadline = Instant::now() + NATIVE_WAIT_TIMEOUT;
            loop {
                if let Some(buf) = self.take(queue) {
                    return Ok(buf);
                }
                let rem = deadline
                    .checked_duration_since(Instant::now())
                    .ok_or_else(|| "coreaudio device is not draining buffers".to_string())?;
                let mut g = self.free.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(buf) = g.pop() {
                    return Ok(buf);
                }
                let (guard, _) = self
                    .cond
                    .wait_timeout(g, rem)
                    .unwrap_or_else(|e| e.into_inner());
                g = guard;
                drop(g);
            }
        }

        fn release(&self, buf: AudioQueueBufferRef) {
            self.free
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(buf);
            self.cond.notify_one();
        }

        /// `true` when every allocated buffer is back on the free list.
        fn drained(&self) -> bool {
            self.free.lock().unwrap_or_else(|e| e.into_inner()).len()
                == *self.allocated.lock().unwrap_or_else(|e| e.into_inner())
        }
    }

    extern "C" fn on_buffer_done(
        user_data: *mut c_void,
        _queue: AudioQueueRef,
        buf: AudioQueueBufferRef,
    ) {
        // SAFETY: `user_data` is the `Pool` leaked by `open`, alive until
        // `AudioQueueDispose` completes and `finish` reclaims it. The
        // callback only locks the pool and returns the buffer pointer.
        unsafe {
            (*(user_data as *const Pool)).release(buf);
        }
    }

    /// AudioQueue-backed sink: buffers are allocated on demand up to
    /// [`NATIVE_MAX_BUFS`], filled from a staging accumulator, and recycled
    /// through the buffer-done callback.
    pub struct CoreAudioSink {
        queue: AudioQueueRef,
        pool: *mut Pool,
        staging: Vec<u8>,
        started: bool,
        finished: bool,
    }

    // SAFETY: all queue/buffer access happens in `push`/`finish` on the
    // owning thread; AudioQueue itself is thread-safe per Apple docs.
    unsafe impl Send for CoreAudioSink {}

    impl CoreAudioSink {
        pub fn open() -> Result<Self, String> {
            let format = AudioStreamBasicDescription {
                sample_rate: super::MIX_SAMPLE_RATE as f64,
                format_id: K_AUDIO_FORMAT_LINEAR_PCM,
                format_flags: PCM_FORMAT_FLAGS,
                bytes_per_packet: 4,
                frames_per_packet: 1,
                bytes_per_frame: 4,
                channels_per_frame: 2,
                bits_per_channel: 16,
                reserved: 0,
            };
            let pool = Box::leak(Box::new(Pool {
                free: Mutex::new(Vec::new()),
                cond: Condvar::new(),
                allocated: Mutex::new(0),
            }));
            let mut queue: AudioQueueRef = std::ptr::null_mut();
            // SAFETY: `format`/`queue` are valid for the call duration;
            // `pool` outlives the queue (leaked, reclaimed in finish).
            let status = unsafe {
                AudioQueueNewOutput(
                    &format,
                    Some(on_buffer_done),
                    pool as *mut Pool as *mut c_void,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    0,
                    &mut queue,
                )
            };
            if status != 0 || queue.is_null() {
                // Reclaim the pool — no queue was created.
                drop(unsafe { Box::from_raw(pool) });
                return Err(format!("AudioQueueNewOutput failed (status {status})"));
            }
            Ok(Self {
                queue,
                pool,
                staging: Vec::new(),
                started: false,
                finished: false,
            })
        }

        fn submit(&mut self, bytes: &[u8]) -> Result<(), String> {
            let buf = unsafe { &*self.pool }.take_wait(self.queue)?;
            // SAFETY: `buf` is an owned pool buffer with capacity
            // `NATIVE_BUF_BYTES`; `bytes` never exceeds it.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    (*buf).audio_data as *mut u8,
                    bytes.len(),
                );
                (*buf).audio_data_size = bytes.len() as u32;
            }
            // SAFETY: `buf` is prepared with exactly `audio_data_size`
            // bytes of valid PCM; CBR streams pass no packet descriptions.
            let status =
                unsafe { AudioQueueEnqueueBuffer(self.queue, buf, 0, std::ptr::null()) };
            if status != 0 {
                unsafe { &*self.pool }.release(buf);
                return Err(format!("AudioQueueEnqueueBuffer failed (status {status})"));
            }
            if !self.started {
                // SAFETY: queue is live; NULL start time = ASAP.
                let status = unsafe { AudioQueueStart(self.queue, std::ptr::null()) };
                if status != 0 {
                    return Err(format!("AudioQueueStart failed (status {status})"));
                }
                self.started = true;
            }
            Ok(())
        }
    }

    impl AudioSink for CoreAudioSink {
        fn push(&mut self, pcm: &[i16]) -> Result<(), String> {
            self.staging.extend_from_slice(&encode_pcm_le(pcm));
            while self.staging.len() >= NATIVE_BUF_BYTES {
                let chunk: Vec<u8> = self.staging.drain(..NATIVE_BUF_BYTES).collect();
                self.submit(&chunk)?;
            }
            Ok(())
        }

        fn label(&self) -> &'static str {
            "coreaudio"
        }

        fn finish(&mut self) {
            if self.finished {
                return;
            }
            self.finished = true;
            // Flush the partial tail, then let the queue play out what is
            // queued (bounded by SINK_DRAIN_TIMEOUT) before disposing.
            if !self.staging.is_empty() {
                let tail = std::mem::take(&mut self.staging);
                let _ = self.submit(&tail);
            }
            if self.started {
                // SAFETY: queue is live; async drain, then bounded wait.
                let _ = unsafe { AudioQueueStop(self.queue, false) };
                let deadline = Instant::now() + SINK_DRAIN_TIMEOUT;
                while !unsafe { &*self.pool }.drained() && Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            // SAFETY: immediate dispose stops the queue and ends callbacks
            // before the pool is reclaimed below.
            let _ = unsafe { AudioQueueDispose(self.queue, true) };
            let pool = unsafe { &*self.pool };
            for buf in pool
                .free
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain(..)
            {
                // SAFETY: queue is disposed; freeing owned buffers.
                let _ = unsafe { AudioQueueFreeBuffer(self.queue, buf) };
            }
            // SAFETY: `pool` was leaked in `open` and the queue — the only
            // other handle on it — is disposed; reclaim it exactly once.
            drop(unsafe { Box::from_raw(self.pool) });
            self.queue = std::ptr::null_mut();
        }
    }

    impl Drop for CoreAudioSink {
        fn drop(&mut self) {
            self.finish();
        }
    }
}

/// Native Windows output through `winmm.dll` `waveOut*` (the classic MME
/// PCM API, present on every Windows since 3.x — no WASAPI/COM needed).
///
/// `unsafe` boundary: FFI declarations and calls are confined to this
/// module behind `#[cfg(target_os = "windows")]`. The sink polls
/// `WHDR_DONE` instead of installing a callback (`CALLBACK_NULL`), so no
/// foreign thread ever dereferences our state. `Box`ed headers keep their
/// addresses stable while the driver owns them; data Vecs' heap buffers
/// are stable for the same reason.
#[cfg(target_os = "windows")]
mod winmm {
    use super::{AudioSink, NATIVE_BUF_BYTES, NATIVE_MAX_BUFS, NATIVE_WAIT_TIMEOUT};
    use super::{encode_pcm_le, SINK_DRAIN_TIMEOUT};
    use std::time::Instant;

    type HWaveOut = *mut std::ffi::c_void;
    type MMResult = u32;

    const MMSYSERR_NOERROR: MMResult = 0;
    const WAVE_MAPPER: u32 = u32::MAX;
    const WAVE_FORMAT_PCM: u16 = 1;
    const WHDR_DONE: u32 = 0x0000_0001;
    const CALLBACK_NULL: u32 = 0;

    #[repr(C)]
    struct WaveFormatEx {
        format_tag: u16,
        channels: u16,
        samples_per_sec: u32,
        avg_bytes_per_sec: u32,
        block_align: u16,
        bits_per_sample: u16,
        cb_size: u16,
    }

    #[repr(C)]
    struct WaveHdr {
        data: *mut u8,
        buffer_length: u32,
        bytes_recorded: u32,
        user: usize,
        flags: u32,
        loops: u32,
        next: *mut WaveHdr,
        reserved: usize,
    }

    #[link(name = "winmm")]
    extern "C" {
        fn waveOutOpen(
            out: *mut HWaveOut,
            device: u32,
            format: *const WaveFormatEx,
            callback: usize,
            instance: usize,
            flags: u32,
        ) -> MMResult;
        fn waveOutPrepareHeader(hwo: HWaveOut, hdr: *mut WaveHdr, size: u32) -> MMResult;
        fn waveOutUnprepareHeader(hwo: HWaveOut, hdr: *mut WaveHdr, size: u32)
            -> MMResult;
        fn waveOutWrite(hwo: HWaveOut, hdr: *mut WaveHdr, size: u32) -> MMResult;
        fn waveOutReset(hwo: HWaveOut) -> MMResult;
        fn waveOutClose(hwo: HWaveOut) -> MMResult;
    }

    /// One in-flight buffer the driver owns until `WHDR_DONE`.
    struct InFlight {
        hdr: Box<WaveHdr>,
        /// Heap-stable PCM data `hdr.data` points into.
        _data: Vec<u8>,
    }

    /// `waveOut*` sink: buffers are prepared+written on demand, polled for
    /// `WHDR_DONE`, and reaped back to Rust on each `push`.
    pub struct WinMmSink {
        handle: HWaveOut,
        in_flight: Vec<InFlight>,
        staging: Vec<u8>,
        finished: bool,
    }

    // SAFETY: the handle and headers are only touched from `push`/`finish`
    // on the owning thread; the driver only reads the pinned buffers.
    unsafe impl Send for WinMmSink {}

    impl WinMmSink {
        pub fn open() -> Result<Self, String> {
            let format = WaveFormatEx {
                format_tag: WAVE_FORMAT_PCM,
                channels: 2,
                samples_per_sec: super::MIX_SAMPLE_RATE,
                avg_bytes_per_sec: super::MIX_SAMPLE_RATE * 4,
                block_align: 4,
                bits_per_sample: 16,
                cb_size: 0,
            };
            let mut handle: HWaveOut = std::ptr::null_mut();
            // SAFETY: `handle`/`format` are valid out/in pointers;
            // CALLBACK_NULL means no callback pointer is ever invoked.
            let status = unsafe {
                waveOutOpen(
                    &mut handle,
                    WAVE_MAPPER,
                    &format,
                    0,
                    0,
                    CALLBACK_NULL,
                )
            };
            if status != MMSYSERR_NOERROR || handle.is_null() {
                return Err(format!("waveOutOpen failed (code {status})"));
            }
            Ok(Self {
                handle,
                in_flight: Vec::new(),
                staging: Vec::new(),
                finished: false,
            })
        }

        /// Unprepare and free every buffer the driver has finished.
        fn reap_done(&mut self) {
            let mut done: Vec<InFlight> = Vec::new();
            let mut i = 0;
            while i < self.in_flight.len() {
                if self.in_flight[i].hdr.flags & WHDR_DONE != 0 {
                    done.push(self.in_flight.remove(i));
                } else {
                    i += 1;
                }
            }
            for mut entry in done {
                // SAFETY: the header was prepared below and the driver
                // signalled completion; unprepare before freeing.
                let _ = unsafe {
                    waveOutUnprepareHeader(
                        self.handle,
                        entry.hdr.as_mut(),
                        std::mem::size_of::<WaveHdr>() as u32,
                    )
                };
            }
        }

        /// Wait for at least one in-flight buffer to finish. `Err` when
        /// the device stalls past [`NATIVE_WAIT_TIMEOUT`].
        fn wait_for_buffer(&mut self) -> Result<(), String> {
            let deadline = Instant::now() + NATIVE_WAIT_TIMEOUT;
            loop {
                self.reap_done();
                if self.in_flight.len() < NATIVE_MAX_BUFS {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err("winmm device is not draining buffers".to_string());
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }

        fn submit(&mut self, bytes: &[u8]) -> Result<(), String> {
            self.wait_for_buffer()?;
            let mut data = bytes.to_vec();
            let mut hdr = Box::new(WaveHdr {
                data: data.as_mut_ptr(),
                buffer_length: data.len() as u32,
                bytes_recorded: 0,
                user: 0,
                flags: 0,
                loops: 0,
                next: std::ptr::null_mut(),
                reserved: 0,
            });
            // SAFETY: `hdr`/`data` are boxed and live until `WHDR_DONE`
            // plus `waveOutUnprepareHeader` in `reap_done`/`finish`.
            let status = unsafe {
                waveOutPrepareHeader(
                    self.handle,
                    hdr.as_mut(),
                    std::mem::size_of::<WaveHdr>() as u32,
                )
            };
            if status != MMSYSERR_NOERROR {
                return Err(format!("waveOutPrepareHeader failed (code {status})"));
            }
            // SAFETY: header is prepared and owned by us; ownership of the
            // buffer passes to the driver until WHDR_DONE.
            let status = unsafe {
                waveOutWrite(
                    self.handle,
                    hdr.as_mut(),
                    std::mem::size_of::<WaveHdr>() as u32,
                )
            };
            if status != MMSYSERR_NOERROR {
                // SAFETY: write never took the buffer; unprepare it back.
                let _ = unsafe {
                    waveOutUnprepareHeader(
                        self.handle,
                        hdr.as_mut(),
                        std::mem::size_of::<WaveHdr>() as u32,
                    )
                };
                return Err(format!("waveOutWrite failed (code {status})"));
            }
            self.in_flight.push(InFlight { hdr, _data: data });
            Ok(())
        }
    }

    impl AudioSink for WinMmSink {
        fn push(&mut self, pcm: &[i16]) -> Result<(), String> {
            self.staging.extend_from_slice(&encode_pcm_le(pcm));
            while self.staging.len() >= NATIVE_BUF_BYTES {
                let chunk: Vec<u8> = self.staging.drain(..NATIVE_BUF_BYTES).collect();
                self.submit(&chunk)?;
            }
            Ok(())
        }

        fn label(&self) -> &'static str {
            "winmm"
        }

        fn finish(&mut self) {
            if self.finished {
                return;
            }
            self.finished = true;
            if !self.staging.is_empty() {
                let tail = std::mem::take(&mut self.staging);
                let _ = self.submit(&tail);
            }
            // Bounded drain: let queued audio play out, then reset and
            // reclaim anything still in flight.
            let deadline = Instant::now() + SINK_DRAIN_TIMEOUT;
            while !self.in_flight.is_empty() && Instant::now() < deadline {
                self.reap_done();
                if !self.in_flight.is_empty() {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            if !self.in_flight.is_empty() {
                // SAFETY: handle is live; reset marks all buffers DONE.
                let _ = unsafe { waveOutReset(self.handle) };
                self.reap_done();
            }
            // Anything the driver never released is unprepared and freed.
            for mut entry in self.in_flight.drain(..) {
                // SAFETY: header was prepared by `submit`; device is reset.
                let _ = unsafe {
                    waveOutUnprepareHeader(
                        self.handle,
                        entry.hdr.as_mut(),
                        std::mem::size_of::<WaveHdr>() as u32,
                    )
                };
            }
            // SAFETY: all buffers are unprepared; the handle closes once.
            let _ = unsafe { waveOutClose(self.handle) };
            self.handle = std::ptr::null_mut();
        }
    }

    impl Drop for WinMmSink {
        fn drop(&mut self) {
            self.finish();
        }
    }
}

/// `pw-cat` playback flags. `--latency=480` requests a 480-sample
/// (≈10 ms at 48 kHz) stream buffer — the low-latency hint from the
/// §3.6 audit lane; PipeWire clamps it to what the device can do, so it
/// is safe on every `pw-cat` version that accepts the flag.
const PW_CAT_ARGS: [&str; 9] = [
    "--playback",
    "--raw",
    "--rate",
    "48000",
    "--channels",
    "2",
    "--format",
    "s16",
    "--latency=480",
];
const APLAY_ARGS: [&str; 10] = [
    "-q", "-f", "S16_LE", "-r", "48000", "-c", "2", "-t", "raw", "-",
];
/// `paplay` raw playback: reads interleaved s16le stereo 48 kHz PCM from
/// stdin when no file is given, so no `-` target is appended. Public so
/// backend tests can pin the exact subprocess contract.
#[doc(hidden)]
pub const PAPLAY_ARGS: [&str; 4] = [
    "--raw",
    "--format=s16le",
    "--rate=48000",
    "--channels=2",
];
const PW_TARGET: [&str; 1] = ["-"];

/// Open one subprocess sink (`pw-cat`, `paplay`, or `aplay`); `None` on
/// spawn failure.
fn try_subprocess(
    prog: &'static str,
    args: &'static [&'static str],
    label: &'static str,
) -> Option<Box<dyn AudioSink>> {
    let mut argv: Vec<&str> = args.to_vec();
    if prog == "pw-cat" {
        argv.extend_from_slice(&PW_TARGET);
    }
    SubprocessSink::spawn(prog, &argv, label)
        .ok()
        .map(|s| Box::new(s) as Box<dyn AudioSink>)
}

/// The label the sink for a kind reports — lets an `auto` retry skip the
/// backend that just died.
fn sink_kind_label(kind: &AudioSinkKind) -> &'static str {
    match kind {
        AudioSinkKind::PipeWire => "pipewire",
        AudioSinkKind::PulseAudio => "pulseaudio",
        AudioSinkKind::Alsa => "alsa",
        AudioSinkKind::CoreAudio => "coreaudio",
        AudioSinkKind::WinMm => "winmm",
        AudioSinkKind::Null | AudioSinkKind::Off => "null",
        _ => "unknown",
    }
}

/// Open one native/subprocess sink candidate; `Err` text on failure so the
/// caller can report which backends were attempted.
fn open_native(kind: &AudioSinkKind) -> Result<Box<dyn AudioSink>, String> {
    match kind {
        AudioSinkKind::PipeWire => try_subprocess("pw-cat", &PW_CAT_ARGS, "pipewire")
            .ok_or_else(|| "pw-cat could not start".to_string()),
        AudioSinkKind::PulseAudio => try_subprocess("paplay", &PAPLAY_ARGS, "pulseaudio")
            .ok_or_else(|| "paplay could not start".to_string()),
        AudioSinkKind::Alsa => try_subprocess("aplay", &APLAY_ARGS, "alsa")
            .ok_or_else(|| "aplay could not start".to_string()),
        AudioSinkKind::CoreAudio => coreaudio_sink(),
        AudioSinkKind::WinMm => winmm_sink(),
        AudioSinkKind::Null | AudioSinkKind::Off => Ok(Box::<NullSink>::default()),
        other => Err(format!("{other:?} is not a native streaming backend")),
    }
}

/// Open a sink for `kind`, returning the sink plus an optional warning.
/// `Auto` walks [`detect_available_sinks_for_os`] for the host OS
/// (`pw-cat`→`paplay`→`aplay` on Linux, CoreAudio on macOS, WinMM on
/// Windows) and
/// falls back to `null` when every candidate is missing or refuses to
/// start — the headless path.
pub fn open_sink(kind: &AudioSinkKind) -> (Box<dyn AudioSink>, Option<String>) {
    match kind {
        AudioSinkKind::Off | AudioSinkKind::Null => (Box::<NullSink>::default(), None),
        AudioSinkKind::Wav(path) => match WavSink::create(path) {
            Ok(sink) => (Box::new(sink), None),
            Err(e) => (
                Box::<NullSink>::default(),
                Some(format!("{e}; falling back to null sink")),
            ),
        },
        AudioSinkKind::Auto => {
            let mut errors = Vec::new();
            for candidate in detect_available_sinks_for_os(host_os(), native_backend_available) {
                match open_native(&candidate) {
                    Ok(sink) => {
                        if sink.label() == "null" && candidate != AudioSinkKind::Null {
                            continue;
                        }
                        let warn = if errors.is_empty() && candidate == AudioSinkKind::Null
                        {
                            Some("no native audio backend on this OS; audio sink is null"
                                .to_string())
                        } else if errors.is_empty() {
                            None
                        } else {
                            Some(format!(
                                "audio backends failed ({}); using {}",
                                errors.join(", "),
                                sink.label()
                            ))
                        };
                        return (sink, warn);
                    }
                    Err(e) => errors.push(format!("{candidate:?}: {e}")),
                }
            }
            (
                Box::<NullSink>::default(),
                Some(format!(
                    "no audio backend available ({}); audio sink is null",
                    errors.join(", ")
                )),
            )
        }
        other => match open_native(other) {
            Ok(sink) => (sink, None),
            Err(e) => (
                Box::<NullSink>::default(),
                Some(format!("{e}; audio sink is null")),
            ),
        },
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
    /// `--gameDir` for the layered sound scan: `mods/*.jar` then
    /// `resourcepacks/*` merge above the vanilla index, exactly like the
    /// vanilla launcher layout (`None` → vanilla assets only).
    pub game_dir: Option<PathBuf>,
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
// Async OGG decode queue — ingest enqueues, a worker decodes off-thread
// ---------------------------------------------------------------------------

/// Decode jobs the worker may have queued — same bounded-queue style as
/// `SOUND_QUEUE_CAP`/`MAX_PENDING_WIRE_EVENTS`: full ⇒ the oldest queued
/// job *for the same source* is evicted to make room, else the new job is
/// declined. Ingest never blocks on decode.
const MAX_DECODE_QUEUE_JOBS: usize = 128;
/// Estimated encoded-byte ceiling for queued decode jobs. Jobs carry a
/// source reference, not the bytes, but the cap keeps a packet flood from
/// queueing minutes of OGG work. A single job larger than the cap is
/// accepted only when the queue is otherwise empty.
const MAX_DECODE_QUEUE_BYTES: usize = 16 * 1024 * 1024;
/// `stop_sound` records retained for in-flight decodes — only recorded
/// while decodes are outstanding; the cap is a backstop for stop spam
/// during a long decode backlog.
const MAX_PENDING_DECODE_STOPS: usize = 256;

/// One decode request: the resolved source plus everything needed to
/// rebuild the `PlaySoundEvent` when PCM lands. Packet scalars are
/// computed on the ingest thread so the result path is a pure submit.
struct DecodeJob {
    /// Ingest order shared with [`PendingDecodeStop`] — a stop recorded
    /// after this job cancels it.
    order: u64,
    /// Source identity for same-source eviction (`ResolvedSound::path`).
    source_key: PathBuf,
    /// Estimated encoded size charged against the queue byte cap.
    size_hint: usize,
    /// Shared library — the worker locks it for the phased decode so the
    /// LRU PCM cache keeps a single owner and ingest `resolve` stays
    /// shared under the read lock.
    library: Arc<RwLock<SoundLibrary>>,
    /// Where to read the OGG bytes from.
    source: SoundSourceData,
    /// Event name — placeholder fallback on decode failure.
    name: String,
    /// `PlaySoundEvent::label`.
    label: String,
    category: i32,
    position: Option<[f32; 3]>,
    entity_id: Option<i32>,
    /// Effective volume (packet × variant, finite-clamped).
    volume: f32,
    /// Clamped pitch multiplier.
    pitch: f32,
    seed: i64,
    max_range_m: f32,
}

/// Worker result: channel-preserving PCM on success (mono downmix plus
/// the optional stereo bypass pair), the decode error string on failure.
enum DecodeOutcome {
    /// Decoded clip in the mixer's `(mono, stereo)` payload shape.
    Decoded {
        /// Mono `f32` at [`MIX_SAMPLE_RATE`] — the 3D/spatial path input.
        mono: Arc<Vec<f32>>,
        /// Original stereo frames for 2D-bypassed voices, when present.
        stereo: Option<Arc<Vec<[f32; 2]>>>,
    },
    /// Decode failed — the runtime falls back to the placeholder tone.
    Failed(String),
}

/// `(job, outcome)` pairs the worker posts back in submission order.
type DecodeResult = (DecodeJob, DecodeOutcome);

/// A `stop_sound` retained while decodes are outstanding — cancels
/// matching [`DecodeJob`]s whose ingest order precedes the stop's, the
/// same category/name rule the mixer applies to already-queued events.
struct PendingDecodeStop {
    /// Shared ingest order — only jobs with a smaller `order` match.
    order: u64,
    category: Option<i32>,
    name: Option<String>,
}

/// Bounded FIFO of [`DecodeJob`]s between ingest and the decode worker.
/// `push` never blocks: when the job/job-byte caps would overflow, the
/// oldest queued job *for the same source* is evicted to make room; when
/// that cannot help the new job is declined. `close` discards the backlog
/// so shutdown stays prompt — at most the in-flight decode is awaited.
struct DecodeQueue {
    inner: Mutex<DecodeQueueInner>,
    not_empty: Condvar,
}

struct DecodeQueueInner {
    jobs: VecDeque<DecodeJob>,
    bytes: usize,
    cap_jobs: usize,
    cap_bytes: usize,
    closed: bool,
    dropped: u64,
}

/// What [`DecodeQueue::push`] did with a job.
struct PushResult {
    /// `true` when the job was queued for the worker.
    accepted: bool,
    /// Same-source jobs evicted to make room — each was counted
    /// outstanding by the runtime and must be un-counted.
    evicted: u64,
}

impl DecodeQueue {
    fn new(cap_jobs: usize, cap_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(DecodeQueueInner {
                jobs: VecDeque::new(),
                bytes: 0,
                cap_jobs,
                cap_bytes,
                closed: false,
                dropped: 0,
            }),
            not_empty: Condvar::new(),
        }
    }

    /// Enqueue `job`, never blocking. Evicts oldest same-source jobs to
    /// make room; declines (counted in [`Self::dropped`]) when even
    /// evicting every queued job for this source cannot fit it. A job
    /// larger than the whole byte cap is accepted only into an empty
    /// queue — the cap bounds backlog, not a lone oversized source.
    fn push(&self, job: DecodeJob) -> PushResult {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.closed {
            g.dropped += 1;
            return PushResult {
                accepted: false,
                evicted: 0,
            };
        }
        let oversized = job.size_hint > g.cap_bytes;
        let mut evicted = 0u64;
        if oversized {
            // A lone oversized job is accepted only into an empty queue —
            // the byte cap bounds backlog, not a single big source.
            if !g.jobs.is_empty() {
                g.dropped += 1;
                return PushResult {
                    accepted: false,
                    evicted: 0,
                };
            }
        } else {
            // Best case: evicting every queued job for this source — if
            // that still does not fit, decline without touching the
            // backlog (evicting other sources' jobs is not allowed).
            let mut bytes_if_cleared = g.bytes;
            let mut len_if_cleared = g.jobs.len();
            for j in &g.jobs {
                if j.source_key == job.source_key {
                    bytes_if_cleared = bytes_if_cleared.saturating_sub(j.size_hint);
                    len_if_cleared -= 1;
                }
            }
            if bytes_if_cleared + job.size_hint > g.cap_bytes
                || len_if_cleared + 1 > g.cap_jobs
            {
                g.dropped += 1;
                return PushResult {
                    accepted: false,
                    evicted: 0,
                };
            }
            // Evict oldest same-source jobs until the new one fits —
            // guaranteed to succeed by the pre-check above.
            while g.bytes + job.size_hint > g.cap_bytes || g.jobs.len() + 1 > g.cap_jobs {
                let Some(pos) = g
                    .jobs
                    .iter()
                    .position(|j| j.source_key == job.source_key)
                else {
                    break;
                };
                if let Some(old) = g.jobs.remove(pos) {
                    g.bytes = g.bytes.saturating_sub(old.size_hint);
                    evicted += 1;
                }
            }
            g.dropped += evicted;
        }
        g.bytes += job.size_hint;
        g.jobs.push_back(job);
        drop(g);
        self.not_empty.notify_one();
        PushResult {
            accepted: true,
            evicted,
        }
    }

    /// Oldest job, or `None` once closed. Waits on the condvar — the
    /// worker is never polled. `close` discards the backlog so shutdown
    /// does not decode sounds nobody will hear.
    fn pop_blocking(&self) -> Option<DecodeJob> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(job) = g.jobs.pop_front() {
                g.bytes = g.bytes.saturating_sub(job.size_hint);
                return Some(job);
            }
            if g.closed {
                return None;
            }
            g = self
                .not_empty
                .wait(g)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Stop accepting and discard the backlog; the worker exits after its
    /// in-flight decode.
    fn close(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.closed = true;
        g.jobs.clear();
        g.bytes = 0;
        drop(g);
        self.not_empty.notify_all();
    }
}

/// Decode worker + result channel. Spawned lazily on the first resolved
/// sound; [`Self::shutdown`] closes the queue (discarding the backlog)
/// and joins — at most one in-flight decode is awaited.
struct DecodePipeline {
    queue: Arc<DecodeQueue>,
    results: mpsc::Receiver<DecodeResult>,
    worker: Option<JoinHandle<()>>,
}

impl DecodePipeline {
    /// Spawn the production pipeline (`decode_one` on a named thread).
    fn spawn() -> Result<Self, String> {
        Self::spawn_with(decode_one)
    }

    /// `spawn` with an injected decoder — tests substitute a blocking or
    /// recording decode to prove ingest never waits on it.
    fn spawn_with(
        decode: impl Fn(&DecodeJob) -> DecodeOutcome + Send + 'static,
    ) -> Result<Self, String> {
        let queue = Arc::new(DecodeQueue::new(
            MAX_DECODE_QUEUE_JOBS,
            MAX_DECODE_QUEUE_BYTES,
        ));
        let (tx, rx) = mpsc::channel::<DecodeResult>();
        let q = queue.clone();
        let worker = std::thread::Builder::new()
            .name("megu-audio-decode".to_string())
            .spawn(move || {
                while let Some(job) = q.pop_blocking() {
                    let outcome = decode(&job);
                    // A gone receiver means the runtime is finished —
                    // exit instead of decoding a dead pipeline's backlog.
                    if tx.send((job, outcome)).is_err() {
                        return;
                    }
                }
            })
            .map_err(|e| format!("cannot spawn audio decode thread: {e}"))?;
        Ok(Self {
            queue,
            results: rx,
            worker: Some(worker),
        })
    }

    /// Close the queue and join the worker. Idempotent; also runs in
    /// `Drop`.
    fn shutdown(&mut self) {
        self.queue.close();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for DecodePipeline {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Repack a decoded clip into the mixer's `(mono, optional stereo)`
/// payload — stereo assets keep L/R for the 2D bypass, everything else
/// consumes the mono downmix.
type SharedPcmPayload = (Arc<Vec<f32>>, Option<Arc<Vec<[f32; 2]>>>);

fn pcm_payload(pcm: &DecodedPcm) -> SharedPcmPayload {
    let stereo = match pcm {
        DecodedPcm::Stereo(v) => Some(Arc::new(v.clone())),
        DecodedPcm::Mono(_) => None,
    };
    (Arc::new(pcm.to_mono()), stereo)
}

/// The production decode step. Cache probe and insert take brief write
/// locks; the expensive OGG decode runs under a *read* lock shared with
/// ingest's `resolve`, so a large decode never stalls the packet path.
fn decode_one(job: &DecodeJob) -> DecodeOutcome {
    if let Some(pcm) = job
        .library
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .cached_pcm(&job.source)
    {
        let (mono, stereo) = pcm_payload(&pcm);
        return DecodeOutcome::Decoded { mono, stereo };
    }
    let decoded = job
        .library
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .decode_uncached(&job.source);
    match decoded {
        Ok(dpcm) => {
            let (mono, stereo) = pcm_payload(&dpcm);
            job.library
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .cache_pcm(&job.source, Arc::new(dpcm));
            DecodeOutcome::Decoded { mono, stereo }
        }
        Err(e) => DecodeOutcome::Failed(e),
    }
}

/// Vanilla mixing scalars shared by the synchronous placeholder path and
/// the async decode job: the packet volume widens the audible range
/// (`attenuation_distance * max(volume, 1.0)`) while output gain still
/// saturates at `min(volume, 1.0)` inside the mixer — so a `volume=4.0`
/// explosion carries 64 m, not 16. An inline `fixed_range` overrides
/// everything, exactly like the wire semantics. Returns
/// `(effective_volume, pitch, max_range_m)`.
fn sound_scalars(
    ev: &WireSoundEvent,
    variant_volume: f32,
    variant_pitch: f32,
    attenuation_distance: Option<f32>,
) -> (f32, f32, f32) {
    let effective_volume = if (ev.volume * variant_volume).is_finite() {
        (ev.volume * variant_volume).max(0.0)
    } else {
        0.0
    };
    let max_range_m = ev.fixed_range.unwrap_or_else(|| {
        attenuation_distance.unwrap_or(16.0) * effective_volume.max(1.0)
    });
    (
        effective_volume,
        (ev.pitch * variant_pitch).clamp(0.5, 2.0),
        max_range_m,
    )
}

// ---------------------------------------------------------------------------
// Subtle armor-equipment foley (fully procedural micro-clinks/rustles)
// ---------------------------------------------------------------------------

/// Vanilla sound-source id for the Players channel. Armor foley is a
/// first-person body sound, so it is governed by the Players volume
/// slider in addition to Master.
pub const SOUND_CATEGORY_PLAYERS: i32 = 7;

/// Base of the caller-assigned `voice_id` space used for foley voices on
/// the dedicated-mix-thread path — high enough never to collide with
/// protocol-derived voice ids (irrelevant for the 2D bypass, but kept
/// stable for acoustic-result bookkeeping).
const FOLEY_VOICE_ID_BASE: u64 = 0xf01e_0000_0000_0000;

/// Armor family a foley cue is synthesized for. [`ArmorFoleyMaterial::None`]
/// is the strict silence case: classification of unarmored/unknown pieces,
/// triggering, and synthesis are all complete no-ops.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ArmorFoleyMaterial {
    /// No audible armor — unarmored, elytra, unrecognized materials.
    #[default]
    None,
    /// Soft cloth/leather rustle.
    Leather,
    /// Light interlocking-mail jingle.
    Chainmail,
    /// Standard dull plate clink.
    Iron,
    /// Brighter, higher plate clink.
    Gold,
    /// Crisp high metallic tick.
    Diamond,
    /// Heavy, low, muted clink.
    Netherite,
}

/// Synthesis recipe for one micro-foley cue: low-gain, tens of
/// milliseconds, no asset file involved. All fields are baked into
/// [`synthesize_armor_foley_pcm`]; the struct is the inspectable contract.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ArmorFoleyCue {
    /// Base loudness in dBFS (`<= 0`), scaled further by trigger intensity.
    pub gain_db: f32,
    /// Resonance/band center in Hz.
    pub center_hz: f32,
    /// Clip length in milliseconds (`35..=60` for every material).
    pub duration_ms: u16,
}

impl ArmorFoleyMaterial {
    /// Per-material synthesis recipe; `None` yields a zero-length,
    /// effectively `-inf` cue so synthesis stays strictly silent.
    pub const fn cue(self) -> ArmorFoleyCue {
        match self {
            Self::None => ArmorFoleyCue {
                gain_db: -120.0,
                center_hz: 0.0,
                duration_ms: 0,
            },
            Self::Leather => ArmorFoleyCue {
                gain_db: -34.0,
                center_hz: 950.0,
                duration_ms: 60,
            },
            Self::Chainmail => ArmorFoleyCue {
                gain_db: -31.0,
                center_hz: 5200.0,
                duration_ms: 48,
            },
            Self::Iron => ArmorFoleyCue {
                gain_db: -28.0,
                center_hz: 3400.0,
                duration_ms: 50,
            },
            Self::Gold => ArmorFoleyCue {
                gain_db: -29.0,
                center_hz: 4200.0,
                duration_ms: 45,
            },
            Self::Diamond => ArmorFoleyCue {
                gain_db: -27.0,
                center_hz: 4900.0,
                duration_ms: 42,
            },
            Self::Netherite => ArmorFoleyCue {
                gain_db: -26.0,
                center_hz: 2800.0,
                duration_ms: 55,
            },
        }
    }

    /// Vanilla armor-tier rank used to pick the dominant material when a
    /// loadout mixes several (`None < Leather < Gold < Chainmail < Iron
    /// < Diamond < Netherite`).
    const fn classify_rank(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Leather => 1,
            Self::Gold => 2,
            Self::Chainmail => 3,
            Self::Iron => 4,
            Self::Diamond => 5,
            Self::Netherite => 6,
        }
    }
}

/// Item-name suffixes that mark an armor slot piece (head / chest / legs /
/// feet). Required for classification so `"iron_golem_spawn_egg"` or
/// `"diamond_pickaxe"` never count as armor.
const ARMOR_SLOT_SUFFIXES: &[&str] = &[
    "helmet",
    "helm",
    "cap",
    "hat",
    "coif",
    "hood",
    "mask",
    "chestplate",
    "tunic",
    "vest",
    "leggings",
    "pants",
    "greaves",
    "trousers",
    "boots",
    "sabatons",
    "shoes",
];

/// `true` when `name` (an item id path, namespace already stripped) ends
/// in a recognized armor-slot word, e.g. `"iron_chestplate"`,
/// `"mail_coif"` or `"steel_greathelm"`. Non-material armor pieces still
/// classify as [`ArmorFoleyMaterial::None`] downstream.
fn name_is_armor_piece(name: &str) -> bool {
    ARMOR_SLOT_SUFFIXES
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

/// Classify a set of equipped armor item ids into the dominant foley
/// material. Ids may be namespaced (`"minecraft:iron_chestplate"`) or
/// bare (`"leather_boots"`); an item only counts when its name ends in a
/// recognized armor-slot word and carries a known material token, so
/// Mod pieces like `"somemod:steel_chainmail_coif"` still resolve while
/// unrelated names (`"iron_golem_spawn_egg"`, `"diamond_pickaxe"`,
/// `"elytra"`) do not. Within one id the material token nearest the slot
/// word wins; across ids the highest vanilla armor tier wins.
pub fn classify_armor_foley(equipped_armor_ids: &[&str]) -> ArmorFoleyMaterial {
    let mut best = ArmorFoleyMaterial::None;
    for id in equipped_armor_ids {
        let name = id.rsplit(':').next().unwrap_or(id);
        if !name_is_armor_piece(name) {
            continue;
        }
        let mut material = ArmorFoleyMaterial::None;
        for token in name.split('_') {
            material = match token {
                t if t.eq_ignore_ascii_case("leather") || t.eq_ignore_ascii_case("hide") => {
                    ArmorFoleyMaterial::Leather
                }
                t if t.eq_ignore_ascii_case("chain")
                    || t.eq_ignore_ascii_case("chainmail")
                    || t.eq_ignore_ascii_case("mail") =>
                {
                    ArmorFoleyMaterial::Chainmail
                }
                t if t.eq_ignore_ascii_case("iron") || t.eq_ignore_ascii_case("steel") => {
                    ArmorFoleyMaterial::Iron
                }
                t if t.eq_ignore_ascii_case("gold") || t.eq_ignore_ascii_case("golden") => {
                    ArmorFoleyMaterial::Gold
                }
                t if t.eq_ignore_ascii_case("diamond") => ArmorFoleyMaterial::Diamond,
                t if t.eq_ignore_ascii_case("netherite") => ArmorFoleyMaterial::Netherite,
                _ => material,
            };
        }
        if material.classify_rank() > best.classify_rank() {
            best = material;
        }
    }
    best
}

/// Splitmix64 — the deterministic bit source for all foley synthesis;
/// no `rand` dependency, reproducible across platforms.
fn foley_next_u64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Deterministic `0..=1` draw off the foley bit source.
fn foley_f01(state: &mut u64) -> f32 {
    ((foley_next_u64(state) >> 40) & 0x00ff_ffff) as f32 / 16_777_215.0
}

/// Synthesize one armor-foley cue as mono s16 PCM at `sample_rate`.
///
/// * `material`: from [`classify_armor_foley`] — `None` returns an empty
///   buffer (strict silence, zero work).
/// * `intensity`: `0..=1` caller estimate of movement speed or landing
///   impact; scales the cue's gain linearly. `0` (or non-finite) returns
///   an empty buffer.
/// * `sample_rate`: output rate; `0` returns an empty buffer, other
///   values are clamped to `8_000..=192_000`.
/// * `seed`: deterministic variation — same inputs reproduce the exact
///   same PCM, different seeds give different phase/transient draws.
///
/// Metals render as a short inharmonic-partial clink with a seeded
/// ±3% detune and a click transient (Chainmail adds a second, lighter
/// jingle); Leather renders as bandpassed noise rustle under a hump
/// envelope. The result is peak-normalized to `<= 0.9` full scale so it
/// can never clip downstream, and is deliberately quiet (`-26..-34` dB
/// base gain): pure-Rust procedural audio, no Mojang asset required.
pub fn synthesize_armor_foley_pcm(
    material: ArmorFoleyMaterial,
    intensity: f32,
    sample_rate: u32,
    seed: u64,
) -> Vec<i16> {
    if matches!(material, ArmorFoleyMaterial::None) || sample_rate == 0 {
        return Vec::new();
    }
    let cue = material.cue();
    let rate = sample_rate.clamp(8_000, 192_000);
    let intensity = if intensity.is_finite() {
        intensity.clamp(0.0, 1.0)
    } else {
        0.0
    };
    if intensity <= 0.0 {
        return Vec::new();
    }
    let n = (usize::from(cue.duration_ms) * rate as usize / 1000).max(1);
    let rate_f = rate as f32;
    let mut rng = seed ^ 0x6a09_e667_f3bc_c909;
    let mut buf = vec![0.0f32; n];

    match material {
        ArmorFoleyMaterial::Leather => {
            // Bandpassed noise rustle: the difference between two one-pole
            // lowpasses behaves like a cheap bandpass around `center_hz`,
            // under a sin^2 hump envelope with a seeded fabric wobble.
            let lp_fast = 1.0 - (-TAU_F * cue.center_hz / rate_f).exp();
            let lp_slow = 1.0 - (-TAU_F * cue.center_hz * 0.20 / rate_f).exp();
            let wobble = 6.0 + 6.0 * foley_f01(&mut rng);
            let mut fast = 0.0f32;
            let mut slow = 0.0f32;
            for (i, s) in buf.iter_mut().enumerate() {
                let x = foley_f01(&mut rng) * 2.0 - 1.0;
                fast += lp_fast * (x - fast);
                slow += lp_slow * (x - slow);
                let t = i as f32 / n as f32;
                let env = PI_F * t;
                let env = env.sin() * env.sin() * (0.8 + 0.2 * (wobble * t).sin());
                *s = (fast - slow) * env * 4.0;
            }
        }
        _ => {
            // Inharmonic plate clink: four decaying partials around
            // `center_hz` with seeded detune, a fast attack and a short
            // noise transient at the strike.
            const RATIOS: [f32; 4] = [1.0, 1.51, 2.42, 3.94];
            const AMPS: [f32; 4] = [1.0, 0.5, 0.24, 0.11];
            let dur = n as f32 / rate_f;
            let mut ratio = [0.0f32; 4];
            let mut phase = [0.0f32; 4];
            for k in 0..4 {
                ratio[k] = RATIOS[k] * (0.97 + 0.06 * foley_f01(&mut rng));
                phase[k] = foley_f01(&mut rng) * TAU_F;
            }
            let echo = matches!(material, ArmorFoleyMaterial::Chainmail);
            for (i, s) in buf.iter_mut().enumerate() {
                let t = i as f32 / rate_f;
                let attack = 1.0 - (-t / 0.0018).exp();
                let mut v = 0.0f32;
                for k in 0..4 {
                    let tau = dur * (0.30 + 0.22 * k as f32);
                    v += AMPS[k]
                        * (TAU_F * cue.center_hz * ratio[k] * t + phase[k]).sin()
                        * (-t / tau).exp();
                }
                if echo {
                    // Mail jingle: a second, lighter clink partway through.
                    let t2 = t - dur * 0.52;
                    if t2 > 0.0 {
                        v += 0.4
                            * (TAU_F * cue.center_hz * 1.33 * t2 + phase[1]).sin()
                            * (-t2 / (dur * 0.18)).exp();
                    }
                }
                let trans = if t < 0.002 {
                    (foley_f01(&mut rng) * 2.0 - 1.0) * 0.12 * (-t / 0.0009).exp()
                } else {
                    0.0
                };
                *s = (v * attack + trans) * 0.6;
            }
        }
    }

    let lin = 10f32.powf(cue.gain_db / 20.0) * intensity;
    let peak = buf.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    let scale = if peak * lin > 0.9 && peak > 0.0 {
        0.9 / peak
    } else {
        lin
    };
    buf.iter()
        .map(|s| ((s * scale).clamp(-1.0, 1.0) * 32_767.0).round() as i16)
        .collect()
}

/// `2.0 * PI` as `f32` — local alias so the synth stays terse.
const TAU_F: f32 = std::f32::consts::TAU;
const PI_F: f32 = std::f32::consts::PI;

// ---------------------------------------------------------------------------
// AudioRuntime — the live pipeline inside ProtocolClient
// ---------------------------------------------------------------------------

/// The live audio pipeline: wire events in, spatialized/limited PCM out.
/// Created once per connection when `AudioSinkKind::Off` is not selected.
pub struct AudioRuntime {
    /// The deterministic mixer (event queue → voices → limited stereo).
    pub mixer: SoundMixer,
    sink: Box<dyn AudioSink>,
    sink_kind: AudioSinkKind,
    /// Shared with the decode worker — `resolve`/size hints take the read
    /// lock, `decode_source`'s cache phases take brief write locks, and
    /// the expensive OGG decode itself runs under the read lock so it
    /// never stalls ingest.
    library: Option<Arc<RwLock<SoundLibrary>>>,
    library_tried: bool,
    assets_dir: Option<PathBuf>,
    asset_index: Option<String>,
    game_dir: Option<PathBuf>,
    subtitle_file: Option<BufWriter<std::fs::File>>,
    subtitles_to_stderr: bool,
    listener_yaw: f32,
    listener_pitch: f32,
    /// `minecraft:sound_event` registry id → name table captured from the
    /// configuration-phase `registry_data` packet (covers every Mod
    /// namespace the server declared); the bundled table stays as the
    /// fallback for sessions that never saw one.
    sound_registry: SoundEventRegistry,
    /// Previous listener feet position + when it was seen — the finite
    /// difference between updates is the listener velocity fed to the
    /// spatializer's Doppler term.
    last_listener_pos: Option<[f64; 3]>,
    last_listener_at: Option<Instant>,
    last_pump: Instant,
    carry_frames: f32,
    scratch_f32: Vec<f32>,
    scratch_i16: Vec<i16>,
    warned_null: bool,
    finished: bool,
    /// Async OGG decode pipeline — `ingest` enqueues and returns; the
    /// worker decodes off-thread and [`Self::drain_decode_results`]
    /// submits results to the mixer in submission order.
    decode: Option<DecodePipeline>,
    /// `true` when the decode thread failed to spawn — ingest falls back
    /// to decoding inline (the pre-queue behavior) rather than dropping
    /// real sounds on the floor.
    decode_spawn_failed: bool,
    /// Decode jobs accepted but not yet drained — backs stop-sound
    /// matching for in-flight decodes.
    decode_outstanding: u64,
    /// Next order shared by decode jobs and retained stop records.
    next_ingest_order: u64,
    /// `stop_sound` records that may still cancel in-flight decodes.
    pending_decode_stops: VecDeque<PendingDecodeStop>,
    /// Dedicated real-time mix thread once
    /// [`AudioRuntime::enable_dedicated_mix_thread`] hands it the sink —
    /// the Render Thread then drives audio purely through the lock-free
    /// command ring (Gate P3), while `pump`/`pump_frames` stay intact for
    /// the synchronous path.
    mix_thread: Option<AudioMixThread>,
    /// Seed counter feeding [`synthesize_armor_foley_pcm`] — each trigger
    /// advances it so successive clinks differ while staying reproducible.
    next_foley_seed: u64,
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
            eprintln!("megu-client audio: {w}");
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
                eprintln!("megu-client audio: cannot write {}: {e}", p.display());
                None
            }
        });
        if cfg.sink != AudioSinkKind::Off {
            eprintln!("megu-client audio: sink={label} (48 kHz s16 stereo)");
        }
        Self {
            mixer,
            sink,
            sink_kind: cfg.sink.clone(),
            library: None,
            library_tried: false,
            assets_dir: cfg.assets_dir.clone(),
            asset_index: cfg.asset_index.clone(),
            game_dir: cfg.game_dir.clone(),
            subtitle_file,
            subtitles_to_stderr: cfg.subtitles_to_stderr,
            listener_yaw: 0.0,
            listener_pitch: 0.0,
            sound_registry: SoundEventRegistry::default(),
            last_listener_pos: None,
            last_listener_at: None,
            last_pump: Instant::now(),
            carry_frames: 0.0,
            scratch_f32: Vec::new(),
            scratch_i16: Vec::new(),
            warned_null: false,
            finished: false,
            decode: None,
            decode_spawn_failed: false,
            decode_outstanding: 0,
            next_ingest_order: 0,
            pending_decode_stops: VecDeque::new(),
            mix_thread: None,
            next_foley_seed: 0,
        }
    }

    /// `true` when a runtime should exist for this config.
    pub fn enabled(cfg: &AudioSessionConfig) -> bool {
        cfg.sink != AudioSinkKind::Off
    }

    /// Lazily load the sound library on first use so a session that never
    /// hears a sound never pays the index parse. When `--gameDir` is set,
    /// `mods/*.jar` then `resourcepacks/*` merge on top of the vanilla
    /// index (lowest→highest priority, matching vanilla layer order). The
    /// loaded library is shared with the decode worker over an `RwLock`.
    fn library(&mut self) -> Option<Arc<RwLock<SoundLibrary>>> {
        if self.library.is_none() && !self.library_tried {
            self.library_tried = true;
            let mut lib = match (&self.assets_dir, &self.asset_index) {
                (Some(dir), Some(index)) => match SoundLibrary::load(dir, index) {
                    Ok(lib) => {
                        eprintln!(
                            "megu-client audio: {} sound events from {}",
                            lib.event_count(),
                            dir.display()
                        );
                        Some(lib)
                    }
                    Err(e) => {
                        eprintln!(
                            "megu-client audio: sound library unavailable ({e}); \
                             placeholder tones will play"
                        );
                        None
                    }
                },
                _ => None,
            };
            if let Some(game_dir) = &self.game_dir {
                // Deterministic layer order: sorted Mod JARs first, then
                // sorted resource packs — packs always win over JAR
                // assets and everything beats the vanilla index.
                let mods_dir = game_dir.join("mods");
                let mut jars = sorted_dir_entries(&mods_dir, "jar");
                jars.extend(sorted_dir_entries(&mods_dir, "zip"));
                let packs_dir = game_dir.join("resourcepacks");
                for path in jars
                    .into_iter()
                    .chain(sorted_dir_entries(&packs_dir, "zip"))
                    .chain(sorted_dir_entries(&packs_dir, "dir"))
                {
                    // No vanilla index? A Mod JAR/pack can still carry the
                    // whole namespace — merge onto an empty library.
                    let lib = lib.get_or_insert_with(SoundLibrary::empty);
                    match lib.merge_sound_layer(&path) {
                        Ok(n) => {
                            eprintln!(
                                "megu-client audio: merged {n} sounds.json from {}",
                                path.display()
                            );
                        }
                        Err(e) => {
                            eprintln!("megu-client audio: {}: {e}", path.display());
                        }
                    }
                }
            }
            self.library = lib.map(|l| Arc::new(RwLock::new(l)));
        }
        self.library.clone()
    }

    /// Install the `minecraft:sound_event` registry table captured from
    /// `registry_data` — covers every id→name mapping the server declared,
    /// including Mod namespaces the bundled table does not know.
    pub fn set_sound_event_registry(&mut self, registry: SoundEventRegistry) {
        self.sound_registry = registry;
    }

    /// Feed a live entity position (`teleport_entity`/`move_entity*` world
    /// tracking applied upstream) so `sound_entity` voices follow it.
    pub fn update_entity_position(&mut self, entity_id: i32, pos: [f64; 3]) {
        self.mixer.set_entity_position(entity_id, pos);
    }

    /// Drop a tracked entity (`remove_entities`/`remove_entity` packet).
    pub fn remove_entity_position(&mut self, entity_id: i32) {
        self.mixer.remove_entity_position(entity_id);
    }

    /// Ingest one decoded `sound_effect`/`sound_entity` packet. Resolution
    /// and scalar math stay on this thread; the OGG decode itself is
    /// handed to the bounded decode worker so a large decode cannot stall
    /// protocol handling — results reach the mixer in submission order
    /// via [`Self::drain_decode_results`].
    pub fn ingest(&mut self, ev: &WireSoundEvent) {
        self.drain_decode_results();
        let (name, label) = match (&ev.name, ev.registry_id) {
            (Some(n), _) => (n.clone(), n.clone()),
            (None, Some(id)) => {
                if let Some(n) = self.sound_registry.name(id) {
                    (n.to_string(), n.to_string())
                } else {
                    match sound_name_for_id(id) {
                        Some(n) => (n.to_string(), n.to_string()),
                        None => {
                            self.mixer.stats.unknown_sound_id += 1;
                            (format!("sound.{id}"), format!("sound.{id}"))
                        }
                    }
                }
            }
            (None, None) => return,
        };

        // Asset resolution: real OGG when the layered sound library maps
        // this event, else a deterministic placeholder tone counted as
        // such. The read lock is shared with the decode worker's
        // long-running decode, so resolve never waits on it.
        let lib = self.library();
        let (resolved, size_hint) = match &lib {
            Some(l) => {
                let g = l.read().unwrap_or_else(|e| e.into_inner());
                let r = g.resolve(&name, ev.seed);
                let hint = r
                    .as_ref()
                    .map_or(0, |res| g.source_size_hint(&res.source));
                if r.is_none() {
                    self.mixer.stats.missing_event_def += 1;
                }
                (r, hint)
            }
            None => {
                self.mixer.stats.missing_assets += 1;
                (None, 0)
            }
        };
        let Some(resolved) = resolved else {
            self.submit_placeholder_sound(&name, &label, ev, 1.0, 1.0, None);
            return;
        };
        let (volume, pitch, max_range_m) = sound_scalars(
            ev,
            resolved.volume,
            resolved.pitch,
            resolved.attenuation_distance,
        );
        let job = DecodeJob {
            // Assigned in `enqueue_decode` from the shared ingest order.
            order: 0,
            source_key: resolved.path.clone(),
            size_hint,
            // `resolved.is_some()` implies the library resolved — it can
            // only come from the `Some` arm above.
            library: lib.expect("resolved sound implies a library"),
            source: resolved.source,
            name,
            label,
            category: ev.category,
            position: ev.position,
            entity_id: ev.entity_id,
            volume,
            pitch,
            seed: ev.seed,
            max_range_m,
        };
        self.enqueue_decode(job);
    }

    /// Ingest one decoded `stop_sound` packet.
    pub fn ingest_stop(&mut self, s: &WireStopSound) {
        self.drain_decode_results();
        self.mixer.submit_stop(s.category, s.name.as_deref());
        // Retain the stop while decodes are outstanding: a matching job
        // still in flight must not start sounding after this stop —
        // the same category/name rule the mixer applies to queued events.
        if self.decode_outstanding > 0 {
            self.next_ingest_order += 1;
            if self.pending_decode_stops.len() >= MAX_PENDING_DECODE_STOPS {
                self.pending_decode_stops.pop_front();
            }
            self.pending_decode_stops.push_back(PendingDecodeStop {
                order: self.next_ingest_order,
                category: s.category,
                name: s.name.clone(),
            });
        }
    }

    /// Queue `job` for the decode worker — never blocks. When the worker
    /// failed to spawn, decode inline instead (the pre-queue behavior) so
    /// a thread-exhausted session still plays real sounds.
    fn enqueue_decode(&mut self, job: DecodeJob) {
        if self.decode.is_none() && !self.decode_spawn_failed {
            match DecodePipeline::spawn() {
                Ok(p) => self.decode = Some(p),
                Err(e) => {
                    self.decode_spawn_failed = true;
                    eprintln!("megu-client audio: {e}; decoding sounds inline");
                }
            }
        }
        let mut job = job;
        self.next_ingest_order += 1;
        job.order = self.next_ingest_order;
        let Some(pipe) = &self.decode else {
            let outcome = decode_one(&job);
            self.finish_decode(job, outcome);
            return;
        };
        let res = pipe.queue.push(job);
        self.decode_outstanding = self
            .decode_outstanding
            .saturating_add(u64::from(res.accepted))
            .saturating_sub(res.evicted);
        if !res.accepted {
            self.mixer.stats.decode_dropped += 1;
        }
        self.mixer.stats.decode_dropped += res.evicted;
    }

    /// Submit every decode result the worker has finished, in submission
    /// order — called from `ingest`, `ingest_stop` and `pump_frames` so
    /// completed sounds reach the mixer without waiting for the next
    /// packet.
    fn drain_decode_results(&mut self) {
        let mut done = Vec::new();
        if let Some(pipe) = &self.decode {
            while let Ok(result) = pipe.results.try_recv() {
                done.push(result);
            }
        }
        for (job, outcome) in done {
            self.decode_outstanding = self.decode_outstanding.saturating_sub(1);
            self.finish_decode(job, outcome);
        }
        if self.decode_outstanding == 0 {
            // No decode can still be cancelled — every future job
            // postdates all recorded stops.
            self.pending_decode_stops.clear();
        }
    }

    /// Complete one decode job: a `stop_sound` recorded after its ingest
    /// cancels it; otherwise submit the decoded PCM, or the placeholder
    /// fallback on failure.
    fn finish_decode(&mut self, job: DecodeJob, outcome: DecodeOutcome) {
        let stopped = self.pending_decode_stops.iter().any(|s| {
            s.order > job.order
                && s.category.is_none_or(|c| c == job.category)
                && s.name
                    .as_deref()
                    .map(normalize_sound_name)
                    .is_none_or(|n| n == normalize_sound_name(&job.label))
        });
        if stopped {
            return;
        }
        match outcome {
            DecodeOutcome::Decoded { mono, stereo } => self.submit_play(PlaySoundEvent {
                label: job.label,
                pcm: mono,
                stereo_pcm: stereo,
                is_placeholder: false,
                category: job.category,
                position: job.position,
                entity_id: job.entity_id,
                volume: job.volume,
                pitch: job.pitch,
                seed: job.seed,
                max_range_m: job.max_range_m,
            }),
            DecodeOutcome::Failed(e) => {
                self.mixer.stats.decode_errors += 1;
                if self.mixer.stats.decode_errors <= 3 {
                    eprintln!("megu-client audio: {e}");
                }
                self.mixer.stats.placeholder_voices += 1;
                self.submit_play(PlaySoundEvent {
                    label: job.label,
                    pcm: Arc::new(placeholder_pcm(
                        &job.name,
                        job.category,
                        self.mixer.sample_rate(),
                    )),
                    stereo_pcm: None,
                    is_placeholder: true,
                    category: job.category,
                    position: job.position,
                    entity_id: job.entity_id,
                    volume: job.volume,
                    pitch: job.pitch,
                    seed: job.seed,
                    max_range_m: job.max_range_m,
                });
            }
        }
    }

    /// Immediate `PlaySoundEvent` for events with no decodable asset —
    /// the deterministic placeholder tone, counted as such.
    fn submit_placeholder_sound(
        &mut self,
        name: &str,
        label: &str,
        ev: &WireSoundEvent,
        variant_volume: f32,
        variant_pitch: f32,
        attenuation_distance: Option<f32>,
    ) {
        let (volume, pitch, max_range_m) =
            sound_scalars(ev, variant_volume, variant_pitch, attenuation_distance);
        self.mixer.stats.placeholder_voices += 1;
        self.submit_play(PlaySoundEvent {
            label: label.to_string(),
            pcm: Arc::new(placeholder_pcm(
                name,
                ev.category,
                self.mixer.sample_rate(),
            )),
            stereo_pcm: None,
            is_placeholder: true,
            category: ev.category,
            position: ev.position,
            entity_id: ev.entity_id,
            volume,
            pitch,
            seed: ev.seed,
            max_range_m,
        });
    }

    /// Hand one finished `PlaySoundEvent` to the mixer — the single place
    /// entity-position accounting and queue submission happen for both
    /// the synchronous placeholder path and async decode results.
    fn submit_play(&mut self, ev: PlaySoundEvent) {
        if let Some(eid) = ev.entity_id {
            if self.mixer.entity_position(eid).is_none() {
                self.mixer.stats.entity_pos_unresolved += 1;
            }
        }
        self.mixer.submit(ev);
    }

    /// Non-blocking trigger for the procedural armor-equipment micro-foley
    /// (see [`synthesize_armor_foley_pcm`]). `intensity` is the caller's
    /// `0..=1` movement-speed/landing-impact estimate; the cue plays on the
    /// Players channel ([`SOUND_CATEGORY_PLAYERS`]) as a centered 2D voice,
    /// so Master and Players gains plus the bus limiter all apply — a
    /// muted session produces silence downstream.
    ///
    /// Strictly a no-op — returns `false` without synthesizing — for
    /// [`ArmorFoleyMaterial::None`], non-positive/non-finite intensity, or
    /// a zero [`AudioLimiter::max_volume_cap`]. On the dedicated-mix-thread
    /// path the cue is posted as a `PlayDecoded` ring command instead;
    /// `false` there means the ring was full.
    pub fn trigger_armor_foley(&mut self, material: ArmorFoleyMaterial, intensity: f32) -> bool {
        if matches!(material, ArmorFoleyMaterial::None)
            || self.mixer.limiter.max_volume_cap() <= 0.0
        {
            return false;
        }
        let intensity = if intensity.is_finite() {
            intensity.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if intensity <= 0.0 {
            return false;
        }
        self.next_foley_seed = self.next_foley_seed.wrapping_add(1);
        let seed = self.next_foley_seed;
        let pcm_i16 = synthesize_armor_foley_pcm(material, intensity, MIX_SAMPLE_RATE, seed);
        if pcm_i16.is_empty() {
            return false;
        }
        let pcm: Vec<f32> = pcm_i16.iter().map(|&s| f32::from(s) / 32_768.0).collect();
        if self.mix_thread.is_some() {
            return self.submit_cmd_nonblocking(AudioCmd::PlayDecoded {
                voice_id: FOLEY_VOICE_ID_BASE + seed,
                pcm: Arc::new(DecodedPcm::Mono(pcm)),
                pos: [0.0; 3],
                entity_id: None,
                volume: 1.0,
                pitch: 1.0,
                attenuation_distance: 0.0,
                is_2d: true,
                category: SOUND_CATEGORY_PLAYERS,
            });
        }
        self.submit_play(PlaySoundEvent {
            label: "megu.armor_foley".to_string(),
            pcm: Arc::new(pcm),
            stereo_pcm: None,
            is_placeholder: false,
            category: SOUND_CATEGORY_PLAYERS,
            position: None,
            entity_id: None,
            volume: 1.0,
            pitch: 1.0,
            seed: seed as i64,
            max_range_m: f32::INFINITY,
        });
        true
    }

    /// Apply the tactile environment's cave/eaves/underwater acoustic hint
    /// (from `tactile_effects::compute_environment_audio_hint`) to every
    /// live and future voice — the synchronous mixer folds it into the
    /// per-voice reverb send and occlusion loss itself.
    pub fn set_environment_hint(&mut self, hint: EnvironmentAudioHint) {
        self.mixer.set_environment_hint(hint);
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
        // Real listener velocity from the pose delta between updates —
        // the spatializer's Doppler term needs a finite m/s estimate, so
        // non-finite or absurd values (>256 m/s) are clamped away.
        let now = Instant::now();
        let velocity = match (self.last_listener_pos, self.last_listener_at) {
            (Some(prev), Some(at)) => {
                let dt = now.saturating_duration_since(at).as_secs_f64().max(1e-3);
                let mut v = [0.0_f32; 3];
                for i in 0..3 {
                    let d = (pos[i] - prev[i]) / dt;
                    v[i] = if d.is_finite() {
                        (d as f32).clamp(-256.0, 256.0)
                    } else {
                        0.0
                    };
                }
                v
            }
            _ => [0.0; 3],
        };
        self.last_listener_pos = Some(pos);
        self.last_listener_at = Some(now);
        self.mixer.set_listener(ListenerPose {
            position: [
                pos[0] as f32,
                (pos[1] + 1.62) as f32, // feet -> eye height
                pos[2] as f32,
            ],
            yaw_deg: self.listener_yaw,
            pitch_deg: self.listener_pitch,
            velocity,
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
        // Completed decodes submit here too — a sound must not wait for
        // the next packet to start.
        self.drain_decode_results();
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
        if let Err(reason) = self.sink.push(&self.scratch_i16) {
            let failed = self.sink.label();
            eprintln!("megu-client audio: {failed} sink failed: {reason}");
            self.sink.finish();
            // An `auto`-chosen subprocess dying mid-stream says nothing
            // about the other backends — walk the remaining candidates
            // once before going silent.
            let mut recovered = false;
            if self.sink_kind == AudioSinkKind::Auto {
                for candidate in
                    detect_available_sinks_for_os(host_os(), native_backend_available)
                {
                    if sink_kind_label(&candidate) == failed {
                        continue;
                    }
                    if let Ok(next) = open_native(&candidate) {
                        eprintln!(
                            "megu-client audio: switched to {} sink",
                            next.label()
                        );
                        self.sink = next;
                        recovered = true;
                        break;
                    }
                }
            }
            if !recovered {
                if !self.warned_null {
                    self.warned_null = true;
                    eprintln!("megu-client audio: continuing on null sink");
                }
                self.sink = Box::<NullSink>::default();
            }
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
                    "megu-client subtitle: {dir} {} ({:.1}m)",
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
        self.mix_thread
            .as_ref()
            .map(AudioMixThread::sink_label)
            .unwrap_or_else(|| self.sink.label())
    }

    /// Hand the sink to a dedicated [`AudioMixThread`] (audit §3.3 /
    /// Gate P3): mixing, spatialization and the shared FDN bus move off
    /// the Render Thread onto the real-time 128-sample loop. Returns the
    /// SPSC command ring the caller posts [`AudioCmd`]s to. Idempotent —
    /// a second call returns the existing ring.
    ///
    /// After this call `pump`/`pump_frames` write to a null sink (the
    /// thread owns the device); keep using `ingest`/`pump` only on the
    /// synchronous path.
    pub fn enable_dedicated_mix_thread(&mut self) -> Arc<AudioCmdRing> {
        if let Some(thread) = &self.mix_thread {
            return thread.ring();
        }
        let sink = std::mem::replace(&mut self.sink, Box::new(NullSink::default()));
        let thread = AudioMixThread::spawn(
            sink,
            &AudioMixThreadOptions {
                preset: SpatialAudioPreset::Realistic,
                sample_rate: MIX_SAMPLE_RATE,
                max_volume_cap: self.mixer.limiter.max_volume_cap(),
            },
        );
        let ring = thread.ring();
        self.mix_thread = Some(thread);
        ring
    }

    /// Non-blocking command submission for the Render Thread — O(1),
    /// lock-free, never panics. `false` when no dedicated mix thread is
    /// enabled or the ring is full (fail-closed; see
    /// [`AudioCmdRing::dropped_commands`]).
    pub fn submit_cmd_nonblocking(&self, cmd: AudioCmd) -> bool {
        self.mix_thread
            .as_ref()
            .is_some_and(|thread| thread.submit(cmd))
    }

    /// The dedicated mix thread handle, when enabled (telemetry access).
    pub fn mix_thread(&self) -> Option<&AudioMixThread> {
        self.mix_thread.as_ref()
    }

    /// Live volume retune from the settings screen: `master` scales every
    /// voice, `categories` is the vanilla `0..=9` source-id gain table,
    /// `max_volume_cap` the bus limiter ceiling. Applies to the
    /// synchronous mixer directly and, when a dedicated
    /// [`AudioMixThread`] owns the sink, forwards the same retune as a
    /// [`AudioCmd::SetVolumes`] so both paths hear the same sliders.
    pub fn set_volume_profile(
        &mut self,
        master: f32,
        categories: [f32; 10],
        max_volume_cap: f32,
    ) {
        self.mixer.set_master_gain(master);
        for (category, gain) in categories.iter().enumerate() {
            self.mixer.set_category_gain(category as i32, *gain);
        }
        self.mixer.limiter.set_max_volume_cap(max_volume_cap);
        let _ = self.submit_cmd_nonblocking(AudioCmd::SetVolumes {
            master,
            categories,
            max_volume_cap,
        });
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
             unknown_id={} missing_event={} decode_err={} decode_drop={} subtitles={}",
            self.sink_label(),
            s.events_submitted,
            s.voices_started,
            s.voices_stolen,
            s.events_suppressed,
            s.placeholder_voices,
            s.unknown_sound_id,
            s.missing_event_def,
            s.decode_errors,
            s.decode_dropped,
            s.subtitle_cues,
        )
    }

    /// Flush the sink and print the report line. Called on drop too.
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        // Stop and join the dedicated mix thread first — it owns the real
        // sink once enabled (`self.sink` is a null stand-in by then).
        self.mix_thread = None;
        // Then the decode worker: the queue is closed (backlog
        // discarded), the in-flight decode finishes, and its result is
        // dropped with the channel — at most one decode is awaited.
        if let Some(mut pipe) = self.decode.take() {
            pipe.shutdown();
        }
        self.sink.finish();
        eprintln!("megu-client audio done: {}", self.report_line());
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

// ---------------------------------------------------------------------------
// Dedicated real-time AudioMixThread (audit §3.3 / Gate P3)
// ---------------------------------------------------------------------------

/// Frames per mix block: 128 at 48 kHz ≈ 2.67 ms — the real-time cadence.
pub const AUDIO_MIX_BLOCK_FRAMES: usize = 128;
/// Interleaved stereo samples per block.
pub const AUDIO_MIX_BLOCK_SAMPLES: usize = AUDIO_MIX_BLOCK_FRAMES * 2;
/// Fixed voice slots owned by the mix thread; admission replaces the oldest
/// slot when all 64 are busy.
pub const AUDIO_MIX_VOICE_SLOTS: usize = 64;
/// Command-ring capacity (power of two) shared between the Render Thread
/// producer and the Audio Mix Thread consumer.
pub const AUDIO_CMD_RING_CAPACITY: usize = 512;
/// Whole block periods a late delivery may lag before it counts starved
/// buffers — the sink's own queue already absorbs sub-period jitter.
const UNDERRUN_GRACE_BLOCKS: u64 = 1;
/// Tail of each deadline wait spent spinning instead of sleeping — OS
/// sleep granularity (≈1 ms) would otherwise land every block late and
/// slowly drift the cadence.
const SPIN_BEFORE_DEADLINE: Duration = Duration::from_micros(400);
/// New voices admitted per block — admission builds a
/// `SpatialAudioEngine` (the only allocation-capable work on the
/// thread), so a burst of `PlayDecoded`s is metered across blocks
/// instead of stalling a single deadline.
const VOICE_ADMIT_PER_BLOCK: usize = 16;

/// Queued `PlayDecoded` awaiting its admission batch.
struct PendingVoice {
    voice_id: u64,
    pcm: Arc<DecodedPcm>,
    pos: [f32; 3],
    entity_id: Option<i32>,
    volume: f32,
    pitch: f32,
    attenuation_distance: f32,
    is_2d: bool,
    /// Vanilla sound-source id (`0..=9`) — `SetVolumes`' per-category
    /// gains index by it exactly like the synchronous mixer does.
    category: i32,
}

/// Lightweight commands the Render Thread posts to the Audio Mix Thread
/// over the lock-free SPSC ring. Decoding, asset I/O and acoustic raycasts
/// all happen upstream (Lane B workers); the mix thread only consumes
/// results — nothing in a command requires I/O or a lock on its side.
#[derive(Debug)]
pub enum AudioCmd {
    /// Listener pose in world space (feet→ear conversion already applied by
    /// the caller, matching `AudioRuntime::update_listener`).
    UpdateListener {
        /// Ear position in block coordinates.
        pos: [f32; 3],
        /// Velocity in blocks/second for Doppler.
        vel: [f32; 3],
        /// Horizontal facing, degrees.
        yaw_deg: f32,
        /// Vertical facing, degrees.
        pitch_deg: f32,
        /// `true` when the listener's ears are submerged.
        submerged: bool,
    },
    /// Play an already-decoded clip. `is_2d` keeps stereo assets on the
    /// bypass path (Music/UI/global); 3D voices spatialize the mono downmix.
    PlayDecoded {
        /// Caller-assigned id used by `ApplyAcousticResult`/`UpdateEntityPos`.
        voice_id: u64,
        /// Decoded payload shared over `Arc` — no copy, no decode here.
        pcm: Arc<DecodedPcm>,
        /// Emission point in block coordinates.
        pos: [f32; 3],
        /// Entity this voice follows, when entity-bound.
        entity_id: Option<i32>,
        /// Packet/resolved volume (may exceed 1.0 — it widens range).
        volume: f32,
        /// Playback-rate multiplier (clamped 0.5..=2.0).
        pitch: f32,
        /// Vanilla attenuation distance in blocks; the mix thread applies
        /// the same `attenuation_distance * max(volume, 1)` range law as
        /// `AudioRuntime::ingest`.
        attenuation_distance: f32,
        /// `true` for the 2D stereo bypass (no spatializer, no reverb send).
        is_2d: bool,
        /// Vanilla sound-source id (`0..=9`) selecting the category gain.
        category: i32,
    },
    /// Live volume retune: master gain, the ten per-category gains, and
    /// the bus limiter ceiling — the dedicated-thread mirror of
    /// `SoundMixer::set_master_gain`/`set_category_gain` plus
    /// `AudioLimiter::set_max_volume_cap`. Gains apply to voices admitted
    /// after the command drains (vanilla applies volume sliders to new
    /// sounds; running voices keep their admission gain).
    SetVolumes {
        /// Master gain `0..=1` applied to every voice.
        master: f32,
        /// Per-source gains `0..=1`, indexed by the vanilla category id.
        categories: [f32; 10],
        /// `qol.max_volume_cap` bus ceiling.
        max_volume_cap: f32,
    },
    /// Latest tracked pose for an entity-bound voice.
    UpdateEntityPos {
        /// Entity being tracked.
        entity_id: i32,
        /// Current position in block coordinates.
        pos: [f32; 3],
        /// Finite velocity in blocks/second, derived upstream.
        vel: [f32; 3],
    },
    /// Acoustic raycast result from a Lane B worker: fold the traced
    /// occlusion loss and reverb send into the running voice's scene. The
    /// mix thread never raycasts itself.
    ApplyAcousticResult {
        /// Voice the result belongs to (`PlayDecoded::voice_id`).
        voice_id: u64,
        /// Effective transmission loss in dB (diffraction already folded).
        occlusion_db: f32,
        /// Late-reverb send level `0..=1`.
        reverb_send: f32,
    },
    /// Stop every sounding voice (Stop Sound with a match-all filter).
    StopAll,
    /// Ask the mix thread to exit its loop cleanly.
    Shutdown,
}

// AudioCmd is Send: every field is plain data or `Arc<DecodedPcm>`
// (`DecodedPcm` is `Send + Sync`), so the ring can carry it across threads.

/// Lock-free single-producer/single-consumer command ring. The producer
/// (Render Thread) calls [`AudioCmdRing::try_push`]; the consumer (Audio
/// Mix Thread) calls [`AudioCmdRing::try_pop`]. Both are O(1), allocate
/// nothing, and never block or panic — a full ring drops the command and
/// counts it instead of stalling the producer.
///
/// SPSC contract: exactly one thread may call `try_push` and exactly one
/// thread may call `try_pop` over the ring's lifetime. `Send`/`Sync` are
/// asserted on that contract: `Release`/`Acquire` ordering pairs the tail
/// store with the consumer's tail load (command payload is visible before
/// the slot is claimed) and vice-versa for `head`.
pub struct AudioCmdRing {
    slots: Box<[UnsafeCell<MaybeUninit<AudioCmd>>]>,
    mask: usize,
    head: AtomicUsize,
    tail: AtomicUsize,
    dropped_cmds: AtomicU64,
}

// SAFETY: sound only under the SPSC contract above — single producer owns
// tail-write/slot-write, single consumer owns head-write/slot-read, and
// Release/Acquire ordering pairs each index publish with the peer's load.
// No slot is ever written twice before being popped (the full check keeps
// producer and consumer indices strictly ordered).
unsafe impl Send for AudioCmdRing {}
// SAFETY: as above — `&self` methods are safe to share across the two
// threads of the SPSC pair.
unsafe impl Sync for AudioCmdRing {}

impl AudioCmdRing {
    /// Empty ring with `capacity` slots; `capacity` must be a power of two
    /// (it becomes the wrap mask).
    pub fn new(capacity: usize) -> Self {
        assert!(capacity.is_power_of_two(), "ring capacity must be a power of two");
        Self {
            slots: (0..capacity)
                .map(|_| UnsafeCell::new(MaybeUninit::uninit()))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            mask: capacity - 1,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            dropped_cmds: AtomicU64::new(0),
        }
    }

    /// Ring capacity in commands.
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Producer side: enqueue `cmd` without blocking. `false` — never a
    /// panic, never a wait — when the ring is full; the command is dropped
    /// and counted in [`Self::dropped_commands`].
    pub fn try_push(&self, cmd: AudioCmd) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail.wrapping_sub(head) >= self.slots.len() {
            self.dropped_cmds.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        // SAFETY: producer is the sole writer; the slot at `tail & mask`
        // is free because `tail - head < len`, and the release store of
        // `tail` below publishes the write to the consumer.
        unsafe {
            (*self.slots[tail & self.mask].get()).write(cmd);
        }
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        true
    }

    /// Consumer side: dequeue the oldest command without blocking;
    /// `None` when the ring is empty.
    pub fn try_pop(&self) -> Option<AudioCmd> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        // SAFETY: consumer is the sole reader; the slot at `head & mask`
        // holds an initialized command because the producer released it
        // before publishing `tail` (acquired above).
        let cmd = unsafe { (*self.slots[head & self.mask].get()).assume_init_read() };
        self.head.store(head.wrapping_add(1), Ordering::Release);
        Some(cmd)
    }

    /// Best-effort snapshot of queued commands (diagnostics only — it can
    /// race with either end).
    pub fn len(&self) -> usize {
        self.tail
            .load(Ordering::Acquire)
            .wrapping_sub(self.head.load(Ordering::Acquire))
            .min(self.slots.len())
    }

    /// `true` when no command is queued (same snapshot caveat as `len`).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Commands the producer dropped because the ring was full.
    pub fn dropped_commands(&self) -> u64 {
        self.dropped_cmds.load(Ordering::Relaxed)
    }
}

/// Atomics shared between the Audio Mix Thread and whatever thread owns
/// the [`AudioMixThread`] handle — the lock-free telemetry surface.
#[derive(Default)]
pub struct AudioMixTelemetry {
    /// Blocks successfully mixed and written to the sink.
    pub blocks_mixed: AtomicU64,
    /// Voice slots currently sounding.
    pub active_voices: AtomicUsize,
    /// Whole block periods missed (device-starved buffers) plus sink write
    /// failures — anything that produced an audible gap.
    pub underruns: AtomicU64,
    /// Ring commands handled on the mix thread.
    pub commands_processed: AtomicU64,
    /// Sink `push` calls that returned an error.
    pub sink_errors: AtomicU64,
}

/// One pre-allocated voice slot. `pcm: None` marks the slot free; slot
/// memory (including an idle `SpatialAudioEngine` once admitted) is
/// retained so the steady-state mix allocates nothing.
struct MixVoiceSlot {
    active: bool,
    /// Monotonic admission order — the steal metric when every slot is busy.
    order: u64,
    voice_id: u64,
    pcm: Option<Arc<DecodedPcm>>,
    /// Fractional read position in `pcm` frames.
    cursor: f64,
    /// Frames advanced per output sample (= clamped pitch).
    step: f64,
    /// Post-clamp mix gain (`volume.clamp(0.0, 1.0)`).
    gain: f32,
    entity_id: Option<i32>,
    /// Spatializer — `Some` for 3D voices, `None` for the 2D bypass.
    engine: Option<SpatialAudioEngine>,
    position: [f32; 3],
}

impl MixVoiceSlot {
    fn empty() -> Self {
        Self {
            active: false,
            order: 0,
            voice_id: 0,
            pcm: None,
            cursor: 0.0,
            step: 1.0,
            gain: 0.0,
            entity_id: None,
            engine: None,
            position: [0.0; 3],
        }
    }

    /// Next `out.len()` mono samples for the spatial path (zeros past the
    /// clip end; stereo assets are downmixed). Linear interpolation on the
    /// fractional pitch cursor — identical to `SoundMixer`'s render.
    fn render_mono(&mut self, out: &mut [f32]) {
        let Some(pcm) = &self.pcm else {
            out.fill(0.0);
            self.active = false;
            return;
        };
        let frames = pcm.frames();
        let mono_at = |i: usize| -> f32 {
            match &**pcm {
                DecodedPcm::Mono(v) => v[i],
                DecodedPcm::Stereo(v) => (v[i][0] + v[i][1]) * 0.5,
            }
        };
        for s in out.iter_mut() {
            let i = self.cursor as usize;
            *s = if i + 1 < frames {
                let frac = (self.cursor - i as f64) as f32;
                let a = mono_at(i);
                a + (mono_at(i + 1) - a) * frac
            } else {
                0.0
            };
            self.cursor += self.step;
        }
        if self.cursor as usize >= frames.saturating_sub(1) {
            self.active = false;
        }
    }

    /// 2D bypass render straight into the stereo mix: stereo keeps L/R,
    /// mono duplicates — no engine, no reverb send.
    fn render_2d(&mut self, out: &mut [f32]) {
        let Some(pcm) = &self.pcm else {
            self.active = false;
            return;
        };
        let frames = pcm.frames();
        let gain = self.gain;
        match &**pcm {
            DecodedPcm::Stereo(v) => {
                for i in 0..out.len() / 2 {
                    let idx = self.cursor as usize;
                    if idx + 1 < frames {
                        let frac = (self.cursor - idx as f64) as f32;
                        let a = v[idx];
                        let b = v[idx + 1];
                        out[2 * i] += (a[0] + (b[0] - a[0]) * frac) * gain;
                        out[2 * i + 1] += (a[1] + (b[1] - a[1]) * frac) * gain;
                    }
                    self.cursor += self.step;
                }
            }
            DecodedPcm::Mono(v) => {
                for i in 0..out.len() / 2 {
                    let idx = self.cursor as usize;
                    if idx + 1 < frames {
                        let frac = (self.cursor - idx as f64) as f32;
                        let s = v[idx] + (v[idx + 1] - v[idx]) * frac;
                        out[2 * i] += s * gain;
                        out[2 * i + 1] += s * gain;
                    }
                    self.cursor += self.step;
                }
            }
        }
        if self.cursor as usize >= frames.saturating_sub(1) {
            self.active = false;
        }
    }
}

/// The deterministic mixing core the dedicated thread drives. Owns the
/// sink, the fixed 64-slot voice array, the shared 8-channel listener FDN
/// bus and every pre-allocated scratch buffer — the per-sample mix loop
/// performs no heap allocation (voice admission, which builds a
/// [`SpatialAudioEngine`], happens only on the command-drain path).
pub struct AudioMixCore {
    sink: Box<dyn AudioSink>,
    sink_failed: bool,
    slots: [MixVoiceSlot; AUDIO_MIX_VOICE_SLOTS],
    reverb_bus: SharedListenerFdnBus,
    listener: ListenerPose,
    /// Latest pose per tracked entity — `UpdateEntityPos` writes it,
    /// `PlayDecoded` reads it to bind entity voices that arrive first.
    entities: BTreeMap<i32, ([f32; 3], [f32; 3])>,
    /// `PlayDecoded`s waiting for their per-block admission batch —
    /// pre-reserved so pushes stay on the bounded control path.
    pending: VecDeque<PendingVoice>,
    order: u64,
    sample_rate: u32,
    preset: SpatialAudioPreset,
    max_volume_cap: f32,
    /// Master + per-category voice gains (`AudioCmd::SetVolumes`) —
    /// folded into a voice's admission gain, matching the synchronous
    /// `SoundMixer` slider semantics.
    master_gain: f32,
    category_gain: [f32; 10],
    /// Pre-allocated block scratch — the zero-alloc steady state.
    mix_f32: [f32; AUDIO_MIX_BLOCK_SAMPLES],
    mono_scratch: [f32; AUDIO_MIX_BLOCK_FRAMES],
    send_scratch: [f32; AUDIO_MIX_BLOCK_FRAMES],
    stereo_scratch: Vec<f32>,
    out_i16: [i16; AUDIO_MIX_BLOCK_SAMPLES],
    /// `AudioCmd::Shutdown` drained — the run loop exits at block end.
    wants_shutdown: bool,
}

impl AudioMixCore {
    /// Core over `sink` at `preset` quality and the `max_volume_cap` bus
    /// ceiling. All fixed buffers are allocated here, never in the mix loop.
    pub fn new(sink: Box<dyn AudioSink>, opts: &AudioMixThreadOptions) -> Self {
        Self {
            sink,
            sink_failed: false,
            slots: std::array::from_fn(|_| MixVoiceSlot::empty()),
            reverb_bus: SharedListenerFdnBus::new(opts.sample_rate),
            listener: ListenerPose::default(),
            entities: BTreeMap::new(),
            pending: VecDeque::with_capacity(AUDIO_CMD_RING_CAPACITY),
            order: 0,
            sample_rate: opts.sample_rate,
            preset: opts.preset,
            max_volume_cap: opts.max_volume_cap,
            master_gain: 1.0,
            category_gain: [1.0; 10],
            mix_f32: [0.0; AUDIO_MIX_BLOCK_SAMPLES],
            mono_scratch: [0.0; AUDIO_MIX_BLOCK_FRAMES],
            send_scratch: [0.0; AUDIO_MIX_BLOCK_FRAMES],
            stereo_scratch: Vec::with_capacity(AUDIO_MIX_BLOCK_SAMPLES),
            out_i16: [0; AUDIO_MIX_BLOCK_SAMPLES],
            wants_shutdown: false,
        }
    }

    fn requires_realtime_cadence(&self) -> bool {
        self.sink.requires_realtime_cadence()
    }

    /// Drain pending ring commands, mix exactly one 128-frame block
    /// (2D bypass + 3D spatialized voices + shared FDN tail + limiter),
    /// and write it to the sink. Deterministic — no clock reads — so unit
    /// tests drive it synchronously for timing/verification.
    pub fn step_one_block(&mut self, ring: &AudioCmdRing, telemetry: &AudioMixTelemetry) {
        // Command drain: bounded by the ring's own capacity; voice
        // admission/retirement (the only allocation-capable work) is
        // confined here — the sample loop below touches pre-allocated
        // state only.
        for _ in 0..AUDIO_CMD_RING_CAPACITY {
            let Some(cmd) = ring.try_pop() else {
                break;
            };
            self.apply(cmd);
            telemetry.commands_processed.fetch_add(1, Ordering::Relaxed);
        }
        // Metered admission: engine construction is the only
        // allocation-capable work on this thread, so a burst is spread
        // over blocks instead of overrunning one deadline.
        for _ in 0..VOICE_ADMIT_PER_BLOCK {
            let Some(voice) = self.pending.pop_front() else {
                break;
            };
            self.admit_voice(voice);
        }
        self.mix_block(telemetry);
    }

    /// Apply one command to voice/listener state (control path).
    fn apply(&mut self, cmd: AudioCmd) {
        match cmd {
            AudioCmd::UpdateListener {
                pos,
                vel,
                yaw_deg,
                pitch_deg,
                submerged,
            } => {
                self.listener = ListenerPose {
                    position: pos,
                    yaw_deg,
                    pitch_deg,
                    velocity: vel,
                    submerged,
                };
                for slot in &mut self.slots {
                    if let Some(engine) = &mut slot.engine {
                        engine.set_listener(self.listener);
                    }
                }
            }
            AudioCmd::PlayDecoded {
                voice_id,
                pcm,
                pos,
                entity_id,
                volume,
                pitch,
                attenuation_distance,
                is_2d,
                category,
            } => self.pending.push_back(PendingVoice {
                voice_id,
                pcm,
                pos,
                entity_id,
                volume,
                pitch,
                attenuation_distance,
                is_2d,
                category,
            }),
            AudioCmd::SetVolumes {
                master,
                categories,
                max_volume_cap,
            } => {
                self.master_gain = if master.is_finite() {
                    master.clamp(0.0, 1.0)
                } else {
                    1.0
                };
                for (slot, gain) in self.category_gain.iter_mut().zip(categories.iter()) {
                    *slot = if gain.is_finite() {
                        gain.clamp(0.0, 1.0)
                    } else {
                        1.0
                    };
                }
                if max_volume_cap.is_finite() {
                    self.max_volume_cap = max_volume_cap.clamp(0.0, 1.0);
                }
            }
            AudioCmd::UpdateEntityPos {
                entity_id,
                pos,
                vel,
            } => {
                self.entities.insert(entity_id, (pos, vel));
                for slot in &mut self.slots {
                    if slot.entity_id == Some(entity_id) {
                        slot.position = pos;
                        if let Some(engine) = &mut slot.engine {
                            engine.set_source(SoundSource {
                                position: pos,
                                velocity: vel,
                            });
                        }
                    }
                }
            }
            AudioCmd::ApplyAcousticResult {
                voice_id,
                occlusion_db,
                reverb_send,
            } => {
                if let Some(engine) = self
                    .slots
                    .iter_mut()
                    .find(|s| s.active && s.voice_id == voice_id)
                    .and_then(|s| s.engine.as_mut())
                {
                    let mut scene = engine.scene().clone();
                    scene.occlusion.transmission_loss_db = if occlusion_db.is_finite() {
                        occlusion_db.max(0.0)
                    } else {
                        0.0
                    };
                    scene.wet_gain = if reverb_send.is_finite() {
                        reverb_send.clamp(0.0, 1.0)
                    } else {
                        scene.wet_gain
                    };
                    engine.set_scene(scene);
                    // `set_scene` restores RT60s but geometry targets
                    // (direct_gain/wet_send/er_gain) only recompute on a
                    // pose update — nudge it with the current listener.
                    engine.set_listener(self.listener);
                }
            }
            AudioCmd::StopAll => {
                self.pending.clear();
                for slot in &mut self.slots {
                    slot.active = false;
                    slot.pcm = None;
                }
            }
            AudioCmd::Shutdown => self.wants_shutdown = true,
        }
    }

    /// Admit one decoded clip into a slot (free slot first, oldest-slot
    /// steal when all 64 are busy). Runs on the command path only.
    fn admit_voice(&mut self, voice: PendingVoice) {
        let PendingVoice {
            voice_id,
            pcm,
            pos,
            entity_id,
            volume,
            pitch,
            attenuation_distance,
            is_2d,
            category,
        } = voice;
        self.order += 1;
        let order = self.order;
        let idx = self
            .slots
            .iter()
            .position(|s| !s.active)
            .unwrap_or_else(|| {
                self.slots
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, s)| s.order)
                    .map(|(i, _)| i)
                    .unwrap_or(0)
            });
        // Entity-bound voices start at the latest tracked pose when the
        // command's position is still the caller's best guess — the same
        // fallback the synchronous mixer applies.
        let (position, velocity) = entity_id
            .and_then(|eid| self.entities.get(&eid).copied())
            .unwrap_or((pos, [0.0; 3]));
        let gain = if volume.is_finite() {
            volume.clamp(0.0, 1.0)
        } else {
            0.0
        };
        // Volume sliders fold in at admission: `master * category[source]`
        // scales the voice gain only — the vanilla range law above keeps
        // using the raw packet volume.
        let gain = (gain
            * self.master_gain
            * self.category_gain[category.clamp(0, 9) as usize])
            .clamp(0.0, 1.0);
        let engine = if is_2d {
            None
        } else {
            let mut engine = SpatialAudioEngine::new(self.sample_rate, self.preset);
            // Vanilla range law: `attenuation_distance * max(volume, 1.0)`.
            engine.set_max_range(attenuation_distance * volume.max(1.0));
            engine.set_source(SoundSource { position, velocity });
            // Default open-air scene until a Lane B worker posts
            // `ApplyAcousticResult` — the engine never raycasts here.
            engine.set_listener(self.listener);
            Some(engine)
        };
        let slot = &mut self.slots[idx];
        *slot = MixVoiceSlot {
            active: true,
            order,
            voice_id,
            pcm: Some(pcm),
            cursor: 0.0,
            step: f64::from(pitch.clamp(0.5, 2.0)),
            gain,
            entity_id,
            engine,
            position,
        };
    }

    /// One 128-frame block: per-voice render → spatialization → shared FDN
    /// tail → bus limiter → s16 conversion → sink write. Zero allocation.
    fn mix_block(&mut self, telemetry: &AudioMixTelemetry) {
        self.mix_f32.fill(0.0);
        self.send_scratch.fill(0.0);
        let mut active = 0usize;
        for i in 0..AUDIO_MIX_VOICE_SLOTS {
            let slot = &mut self.slots[i];
            if !slot.active {
                continue;
            }
            if slot.engine.is_some() {
                slot.render_mono(&mut self.mono_scratch);
                if let Some(engine) = &mut slot.engine {
                    engine.process_shared(
                        &self.mono_scratch,
                        &mut self.stereo_scratch,
                        &mut self.send_scratch,
                        slot.gain,
                    );
                }
                let gain = slot.gain;
                for f in 0..AUDIO_MIX_BLOCK_FRAMES {
                    self.mix_f32[2 * f] += self.stereo_scratch[2 * f] * gain;
                    self.mix_f32[2 * f + 1] += self.stereo_scratch[2 * f + 1] * gain;
                }
            } else {
                slot.render_2d(&mut self.mix_f32);
            }
            if slot.active {
                active += 1;
            }
        }
        // The shared bus runs even with zero voices so a finishing tail
        // decays instead of cutting off — same semantics as `SoundMixer`.
        for f in 0..AUDIO_MIX_BLOCK_FRAMES {
            let (wet_l, wet_r) = self.reverb_bus.step_input(self.send_scratch[f]);
            self.mix_f32[2 * f] += wet_l;
            self.mix_f32[2 * f + 1] += wet_r;
        }
        AudioLimiter::limit_mix_buffer(&mut self.mix_f32, self.max_volume_cap);
        for (i, &s) in self.mix_f32.iter().enumerate() {
            self.out_i16[i] = (s.clamp(-1.0, 1.0) * 32_767.0).round() as i16;
        }
        telemetry.active_voices.store(active, Ordering::Relaxed);
        if self.sink.push(&self.out_i16).is_err() {
            // A sink that cannot accept the block starved the device:
            // count it as an underrun and fail closed to the null sink,
            // exactly like `AudioRuntime::pump_frames`.
            telemetry.sink_errors.fetch_add(1, Ordering::Relaxed);
            if !self.sink_failed {
                self.sink_failed = true;
                telemetry.underruns.fetch_add(1, Ordering::Relaxed);
                self.sink.finish();
                self.sink = Box::<NullSink>::default();
            }
            let _ = self.sink.push(&self.out_i16);
        }
        telemetry.blocks_mixed.fetch_add(1, Ordering::Relaxed);
    }
}

/// Tunables for [`AudioMixThread`]/[`AudioMixCore`].
#[derive(Clone, Copy, Debug)]
pub struct AudioMixThreadOptions {
    /// Mix rate — must match the sink (`48_000`).
    pub sample_rate: u32,
    /// Voice spatialization quality tier.
    pub preset: SpatialAudioPreset,
    /// Bus limiter ceiling (`qol.max_volume_cap`).
    pub max_volume_cap: f32,
}

impl Default for AudioMixThreadOptions {
    fn default() -> Self {
        Self {
            sample_rate: MIX_SAMPLE_RATE,
            preset: SpatialAudioPreset::Realistic,
            max_volume_cap: 1.0,
        }
    }
}

/// The dedicated real-time audio mix thread (audit §3.3 / Gate P3). The
/// Render Thread never mixes: it posts [`AudioCmd`]s to the SPSC ring with
/// [`AudioMixThread::submit`], and the mix thread consumes them on a fixed
/// 128-sample (≈2.67 ms) cadence, so a stalled render pass can never starve
/// the device.
///
/// Two drive modes:
/// * [`AudioMixThread::spawn`] — the real-time mode: a named OS thread
///   runs `step_one_block` on the deadline-anchored cadence above.
/// * [`AudioMixThread::manual`] — the deterministic mode: no OS thread;
///   the caller drives [`AudioMixThread::step_one_block`] synchronously.
pub struct AudioMixThread {
    ring: Arc<AudioCmdRing>,
    telemetry: Arc<AudioMixTelemetry>,
    shutdown: Arc<AtomicBool>,
    /// `Some` in manual mode until `start` moves it onto the OS thread.
    core: Option<AudioMixCore>,
    handle: Option<JoinHandle<()>>,
    sink_label: &'static str,
    period: Duration,
}

impl AudioMixThread {
    /// Spawn the mix thread over `sink`. The thread owns the sink from
    /// here on; `Drop` pushes `Shutdown`, sets the exit flag and joins.
    pub fn spawn(sink: Box<dyn AudioSink>, opts: &AudioMixThreadOptions) -> Self {
        let mut this = Self::manual(sink, opts);
        // Spawn can only fail on OS thread exhaustion — surface it by
        // falling back to manual mode (the caller still gets a working,
        // caller-driven mixer rather than a panic on the render path).
        let _ = this.start();
        this
    }

    /// Deterministic manual mode: ring + core exist, no OS thread yet.
    /// Drive with [`Self::step_one_block`], or promote with [`Self::start`].
    pub fn manual(sink: Box<dyn AudioSink>, opts: &AudioMixThreadOptions) -> Self {
        let sink_label = sink.label();
        Self {
            ring: Arc::new(AudioCmdRing::new(AUDIO_CMD_RING_CAPACITY)),
            telemetry: Arc::new(AudioMixTelemetry::default()),
            shutdown: Arc::new(AtomicBool::new(false)),
            core: Some(AudioMixCore::new(sink, opts)),
            handle: None,
            sink_label,
            period: Duration::from_secs_f64(
                AUDIO_MIX_BLOCK_FRAMES as f64 / f64::from(opts.sample_rate.max(1)),
            ),
        }
    }

    /// Promote manual mode onto a named OS thread running the fixed
    /// 128-sample cadence. `Err` when the thread cannot spawn or the core
    /// was already moved to a thread.
    pub fn start(&mut self) -> Result<(), String> {
        let Some(core) = self.core.take() else {
            return Err("audio mix thread already started".to_string());
        };
        let ring = self.ring.clone();
        let telemetry = self.telemetry.clone();
        let shutdown = self.shutdown.clone();
        let period = self.period;
        // One-shot handoff: the core reaches the OS thread only after a
        // successful spawn — a failed spawn keeps it for manual mode.
        let (tx, rx) = std::sync::mpsc::channel::<AudioMixCore>();
        match std::thread::Builder::new()
            .name("megu-audio-mix".to_string())
            .spawn(move || {
                let Ok(mut core) = rx.recv() else {
                    return;
                };
                Self::run(&mut core, &ring, &telemetry, &shutdown, period);
            }) {
            Ok(handle) => {
                let _ = tx.send(core);
                self.handle = Some(handle);
                Ok(())
            }
            Err(e) => {
                self.core = Some(core);
                Err(format!("cannot spawn audio mix thread: {e}"))
            }
        }
    }

    /// Real-time loop: deadline-anchored so one slow block does not drift
    /// the cadence; a block delivered more than [`UNDERRUN_GRACE_BLOCKS`]
    /// whole periods late counts the extra periods as underruns — the
    /// buffers the device starved on — and the schedule skips ahead.
    fn run(
        core: &mut AudioMixCore,
        ring: &AudioCmdRing,
        telemetry: &AudioMixTelemetry,
        shutdown: &AtomicBool,
        period: Duration,
    ) {
        let period_nanos = period.as_nanos();
        let mut next_due = Instant::now() + period;
        loop {
            // Wait for the deadline: coarse sleep, then a bounded spin so
            // the block lands on time instead of one tick late.
            loop {
                let now = Instant::now();
                if now >= next_due {
                    break;
                }
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
                let wait = next_due - now;
                if wait > SPIN_BEFORE_DEADLINE {
                    std::thread::sleep(wait - SPIN_BEFORE_DEADLINE);
                } else {
                    std::hint::spin_loop();
                }
            }
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            let now = Instant::now();
            let starved = (now.saturating_duration_since(next_due).as_nanos()
                / period_nanos) as u64;
            if core.requires_realtime_cadence() && starved > UNDERRUN_GRACE_BLOCKS {
                telemetry
                    .underruns
                    .fetch_add(starved - UNDERRUN_GRACE_BLOCKS, Ordering::Relaxed);
            }
            core.step_one_block(ring, telemetry);
            // Skip the slots the device already starved through — one
            // period per block keeps the schedule from compounding.
            next_due += period * (starved + 1) as u32;
            if core.wants_shutdown {
                break;
            }
        }
    }

    /// Deterministic single-block step for unit/timing verification —
    /// manual mode only; `false` once the core lives on the OS thread.
    pub fn step_one_block(&mut self) -> bool {
        let Some(core) = self.core.as_mut() else {
            return false;
        };
        core.step_one_block(&self.ring, &self.telemetry);
        true
    }

    /// Non-blocking command submission for the Render Thread — O(1),
    /// lock-free, never panics. `false` when the ring is full (command
    /// dropped, counted in [`Self::dropped_commands`]).
    pub fn submit(&self, cmd: AudioCmd) -> bool {
        self.ring.try_push(cmd)
    }

    /// The shared ring handle — post commands with
    /// [`AudioCmdRing::try_push`] from the producer thread.
    pub fn ring(&self) -> Arc<AudioCmdRing> {
        self.ring.clone()
    }

    /// Backend label of the sink the thread owns.
    pub fn sink_label(&self) -> &'static str {
        self.sink_label
    }

    /// `true` while the OS thread is running (manual mode reports `false`
    /// until [`Self::start`]).
    pub fn is_running(&self) -> bool {
        self.handle
            .as_ref()
            .is_some_and(|h| !h.is_finished())
    }

    /// Blocks mixed and written to the sink.
    pub fn blocks_mixed(&self) -> u64 {
        self.telemetry.blocks_mixed.load(Ordering::Relaxed)
    }

    /// Voice slots currently sounding.
    pub fn active_voice_count(&self) -> usize {
        self.telemetry.active_voices.load(Ordering::Relaxed)
    }

    /// Starved device buffers + sink write failures so far.
    pub fn underrun_count(&self) -> u64 {
        self.telemetry.underruns.load(Ordering::Relaxed)
    }

    /// Ring commands handled on the mix thread.
    pub fn commands_processed(&self) -> u64 {
        self.telemetry.commands_processed.load(Ordering::Relaxed)
    }

    /// Producer-side drops when the ring was full.
    pub fn dropped_commands(&self) -> u64 {
        self.ring.dropped_commands()
    }

    /// Ask the thread to stop and join it. Idempotent; also runs in `Drop`.
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // Best-effort: the flag above is authoritative, so a flooded ring
        // can never hang shutdown.
        let _ = self.ring.try_push(AudioCmd::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for AudioMixThread {
    fn drop(&mut self) {
        self.shutdown();
    }
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
    fn pcm_queue_pop_blocking_survives_idle_silence_until_closed() {
        let q = Arc::new(PcmQueue::new(64));
        let q_rx = q.clone();
        let reader = std::thread::spawn(move || {
            let first = q_rx.pop_blocking();
            let second = q_rx.pop_blocking();
            (first, second)
        });
        // Sleep past the old 100 ms timeout before pushing: the writer must
        // stay open instead of treating silence as EOF.
        std::thread::sleep(Duration::from_millis(130));
        q.push(vec![42, 43]);
        q.close();
        let (first, second) = reader.join().unwrap();
        assert_eq!(first, Some(vec![42, 43]));
        assert_eq!(second, None);
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

    // ------------------------------------------------------------------
    // Async decode queue (ledger L26)
    // ------------------------------------------------------------------

    /// Minimal [`DecodeJob`] for queue/pipeline tests — the library is an
    /// empty `SoundLibrary` since injected decoders never touch it.
    fn test_decode_job(order: u64, key: &str, size_hint: usize) -> DecodeJob {
        DecodeJob {
            order,
            source_key: PathBuf::from(key),
            size_hint,
            library: Arc::new(RwLock::new(SoundLibrary::empty())),
            source: SoundSourceData::File(PathBuf::from(format!("/{key}.ogg"))),
            name: key.to_string(),
            label: key.to_string(),
            category: 0,
            position: None,
            entity_id: None,
            volume: 1.0,
            pitch: 1.0,
            seed: 0,
            max_range_m: f32::INFINITY,
        }
    }

    fn decoded_outcome() -> DecodeOutcome {
        DecodeOutcome::Decoded {
            mono: Arc::new(vec![0.0; 8]),
            stereo: None,
        }
    }

    #[test]
    fn decode_queue_is_fifo_and_close_discards_backlog() {
        let q = DecodeQueue::new(8, 1 << 20);
        for i in 0..3 {
            let r = q.push(test_decode_job(i, &format!("s{i}"), 10));
            assert!(r.accepted);
        }
        assert_eq!(q.pop_blocking().unwrap().order, 0);
        q.close();
        // The remaining backlog is discarded, not drained.
        assert!(q.pop_blocking().is_none());
        assert!(!q.push(test_decode_job(9, "late", 10)).accepted);
        let g = q.inner.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(g.dropped, 1, "only the post-close push counts as dropped");
    }

    #[test]
    fn decode_queue_cap_evicts_same_source_then_declines() {
        // Byte cap 100: a(30)+b(30)+a(30)+b(30) = 120 overflows, so the
        // fourth push evicts the oldest same-source job ("b" order 2).
        let q = DecodeQueue::new(8, 100);
        for (order, key) in [(1, "a"), (2, "b"), (3, "a"), (4, "b")] {
            assert!(q.push(test_decode_job(order, key, 30)).accepted);
        }
        let mut orders = Vec::new();
        for _ in 0..3 {
            orders.push(q.pop_blocking().unwrap().order);
        }
        assert_eq!(orders, vec![1, 3, 4], "oldest same-source job evicted");
        q.close();

        // Refill to 90/100 bytes across three sources, then a fourth
        // unrelated source cannot fit — decline without evicting.
        let q = DecodeQueue::new(8, 100);
        for (order, key) in [(10, "a"), (11, "b"), (12, "c")] {
            assert!(q.push(test_decode_job(order, key, 30)).accepted);
        }
        let r = q.push(test_decode_job(13, "d", 30));
        assert!(!r.accepted, "unrelated-source job must be declined");
        assert_eq!(r.evicted, 0, "decline must not evict other sources");
        let g = q.inner.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(g.jobs.len(), 3);
        drop(g);
        q.close();
    }

    #[test]
    fn decode_queue_job_cap_evicts_only_same_source() {
        let q = DecodeQueue::new(2, 1 << 20);
        assert!(q.push(test_decode_job(1, "a", 1)).accepted);
        assert!(q.push(test_decode_job(2, "b", 1)).accepted);
        // "c" fits nowhere: evicting same-source (none) can't help.
        assert!(!q.push(test_decode_job(3, "c", 1)).accepted);
        // "a" again: evicts oldest "a" (order 1), keeps "b".
        let r = q.push(test_decode_job(4, "a", 1));
        assert!(r.accepted);
        assert_eq!(r.evicted, 1);
        assert_eq!(q.pop_blocking().unwrap().order, 2);
        assert_eq!(q.pop_blocking().unwrap().order, 4);
        q.close();
    }

    #[test]
    fn decode_pipeline_returns_results_in_submission_order() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_rx = seen.clone();
        let mut pipe = DecodePipeline::spawn_with(move |job| {
            seen_rx.lock().unwrap().push(job.order);
            decoded_outcome()
        })
        .unwrap();
        for i in 0..5u64 {
            assert!(pipe.queue.push(test_decode_job(i, &format!("s{i}"), 1)).accepted);
        }
        let mut got = Vec::new();
        for _ in 0..5 {
            let (job, _outcome) = pipe
                .results
                .recv_timeout(Duration::from_secs(5))
                .expect("worker must answer every job");
            got.push(job.order);
        }
        pipe.shutdown();
        assert_eq!(got, vec![0, 1, 2, 3, 4], "results must stay in order");
        assert_eq!(*seen.lock().unwrap(), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn decode_pipeline_shutdown_discards_backlog_promptly() {
        let decodes = Arc::new(AtomicUsize::new(0));
        let d = decodes.clone();
        let mut pipe = DecodePipeline::spawn_with(move |_job| {
            d.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(150));
            decoded_outcome()
        })
        .unwrap();
        for i in 0..8u64 {
            assert!(pipe.queue.push(test_decode_job(i, &format!("s{i}"), 1)).accepted);
        }
        // Let the worker pick up the first job, then close: the backlog
        // must be discarded — shutdown waits on at most the in-flight
        // decode, never the whole queue.
        std::thread::sleep(Duration::from_millis(50));
        let t = Instant::now();
        pipe.shutdown();
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "shutdown blocked on backlog: {:?}",
            t.elapsed()
        );
        assert!(
            decodes.load(Ordering::Relaxed) <= 2,
            "backlog decoded anyway: {}",
            decodes.load(Ordering::Relaxed)
        );
    }

    /// Minimal asset fixture: one `test.ping` event resolving to a plain
    /// file source (contents are never decoded — tests inject the
    /// decoder).
    fn decode_test_assets() -> PathBuf {
        // Parallel tests share the process — the fixture dir must be
        // unique per call or one test's `remove_dir_all` races another's
        // library load.
        static FIXTURE_SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mc-decode-fix-{}-{}",
            std::process::id(),
            FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("indexes")).unwrap();
        let h1 = "aa".repeat(20);
        let h3 = "cc".repeat(20);
        for h in [&h1, &h3] {
            std::fs::create_dir_all(dir.join(format!("objects/{}", &h[..2]))).unwrap();
        }
        std::fs::write(dir.join(format!("objects/{}/{}", &h1[..2], h1)), b"ogg-bytes").unwrap();
        std::fs::write(
            dir.join(format!("objects/{}/{}", &h3[..2], h3)),
            br#"{"test.ping": {"sounds": [{"name": "test/ping1"}]}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("indexes/fix.json"),
            format!(
                r#"{{"objects": {{
                    "minecraft/sounds/test/ping1.ogg": {{"hash": "{h1}", "size": 9}},
                    "minecraft/sounds.json": {{"hash": "{h3}", "size": 300}}
                }}}}"#
            ),
        )
        .unwrap();
        dir
    }

    fn decode_test_runtime() -> AudioRuntime {
        let dir = decode_test_assets();
        let cfg = AudioSessionConfig {
            sink: AudioSinkKind::Null,
            assets_dir: Some(dir),
            asset_index: Some("fix".to_string()),
            game_dir: None,
            max_volume_cap: 1.0,
            max_concurrent_same_sound: 4,
            subtitle_path: None,
            subtitles_to_stderr: false,
        };
        AudioRuntime::new(&cfg)
    }

    fn ping_event() -> WireSoundEvent {
        WireSoundEvent {
            name: Some("test.ping".to_string()),
            registry_id: None,
            category: 0,
            position: Some([1.0, 64.0, 1.0]),
            entity_id: None,
            volume: 1.0,
            pitch: 1.0,
            seed: 0,
            fixed_range: None,
        }
    }

    /// A decoder parked on a channel gate: each call waits for one token,
    /// so a test controls exactly when each decode finishes.
    fn gated_pipeline() -> (DecodePipeline, mpsc::Sender<()>, Arc<AtomicUsize>) {
        let (tx, rx) = mpsc::channel::<()>();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let pipe = DecodePipeline::spawn_with(move |_job| {
            c.fetch_add(1, Ordering::Relaxed);
            let _ = rx.recv();
            decoded_outcome()
        })
        .unwrap();
        (pipe, tx, calls)
    }

    #[test]
    fn ingest_returns_before_decode_finishes_then_submits_in_order() {
        let mut rt = decode_test_runtime();
        let (pipe, gate, calls) = gated_pipeline();
        rt.decode = Some(pipe);

        let t = Instant::now();
        rt.ingest(&ping_event());
        rt.ingest(&ping_event());
        let elapsed = t.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "ingest blocked on decode: {elapsed:?}"
        );
        assert_eq!(rt.decode_outstanding, 2, "both jobs queued, none drained");
        assert_eq!(rt.mixer.voice_count(), 0, "nothing may submit early");

        // Release the first decode only: its result must reach the mixer
        // while the second job is still parked.
        gate.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            rt.pump_frames(64);
            if rt.stats().events_submitted >= 1 {
                break;
            }
            assert!(Instant::now() < deadline, "first decode never landed");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(rt.stats().events_submitted, 1);
        assert_eq!(rt.decode_outstanding, 1);

        gate.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            rt.pump_frames(64);
            if rt.stats().events_submitted >= 2 {
                break;
            }
            assert!(Instant::now() < deadline, "second decode never landed");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        rt.finish();
    }

    // ------------------------------------------------------------------
    // Armor foley
    // ------------------------------------------------------------------

    #[test]
    fn classify_armor_foley_picks_highest_tier_present() {
        assert_eq!(classify_armor_foley(&[]), ArmorFoleyMaterial::None);
        assert_eq!(
            classify_armor_foley(&["minecraft:leather_boots", "leather_cap"]),
            ArmorFoleyMaterial::Leather
        );
        assert_eq!(
            classify_armor_foley(&["chainmail_chestplate"]),
            ArmorFoleyMaterial::Chainmail
        );
        assert_eq!(
            classify_armor_foley(&["golden_leggings"]),
            ArmorFoleyMaterial::Gold
        );
        // Mixed loadouts resolve to the highest vanilla armor tier.
        assert_eq!(
            classify_armor_foley(&[
                "leather_helmet",
                "minecraft:iron_chestplate",
                "leather_boots"
            ]),
            ArmorFoleyMaterial::Iron
        );
        assert_eq!(
            classify_armor_foley(&["netherite_boots", "diamond_chestplate"]),
            ArmorFoleyMaterial::Netherite
        );
        assert_eq!(
            classify_armor_foley(&["iron_helmet", "chainmail_leggings"]),
            ArmorFoleyMaterial::Iron
        );
        // Mod namespaces and compound names still resolve by token.
        assert_eq!(
            classify_armor_foley(&["somemod:steel_chainmail_coif"]),
            ArmorFoleyMaterial::Chainmail
        );
        // Non-armor / unknown materials stay silent.
        assert_eq!(
            classify_armor_foley(&["turtle_helmet", "elytra", "carved_pumpkin"]),
            ArmorFoleyMaterial::None
        );
        // Token matching: "iron" inside unrelated names does not count.
        assert_eq!(
            classify_armor_foley(&["iron_golem_spawn_egg"]),
            ArmorFoleyMaterial::None
        );
    }

    #[test]
    fn armor_foley_cue_is_quiet_and_in_spec_duration() {
        for m in [
            ArmorFoleyMaterial::Leather,
            ArmorFoleyMaterial::Chainmail,
            ArmorFoleyMaterial::Iron,
            ArmorFoleyMaterial::Gold,
            ArmorFoleyMaterial::Diamond,
            ArmorFoleyMaterial::Netherite,
        ] {
            let cue = m.cue();
            assert!(cue.gain_db <= -20.0, "foley must stay subtle: {cue:?}");
            assert!(
                (35..=60).contains(&cue.duration_ms),
                "duration out of spec: {cue:?}"
            );
            assert!(cue.center_hz > 0.0);
        }
        let none = ArmorFoleyMaterial::None.cue();
        assert_eq!(none.duration_ms, 0);
    }

    #[test]
    fn synthesize_armor_foley_pcm_is_silent_for_none_and_zero_intensity() {
        assert!(synthesize_armor_foley_pcm(ArmorFoleyMaterial::None, 1.0, 48_000, 7).is_empty());
        for m in [
            ArmorFoleyMaterial::Iron,
            ArmorFoleyMaterial::Leather,
            ArmorFoleyMaterial::Netherite,
        ] {
            assert!(synthesize_armor_foley_pcm(m, 0.0, 48_000, 7).is_empty());
            assert!(synthesize_armor_foley_pcm(m, f32::NAN, 48_000, 7).is_empty());
            assert!(synthesize_armor_foley_pcm(m, -1.0, 48_000, 7).is_empty());
            assert!(synthesize_armor_foley_pcm(m, 1.0, 0, 7).is_empty());
        }
    }

    #[test]
    fn synthesize_armor_foley_pcm_is_bounded_finite_and_non_clipping() {
        for m in [
            ArmorFoleyMaterial::Iron,
            ArmorFoleyMaterial::Chainmail,
            ArmorFoleyMaterial::Leather,
            ArmorFoleyMaterial::Gold,
            ArmorFoleyMaterial::Diamond,
            ArmorFoleyMaterial::Netherite,
        ] {
            let pcm = synthesize_armor_foley_pcm(m, 1.0, 48_000, 42);
            let expected = usize::from(m.cue().duration_ms) * 48;
            assert_eq!(pcm.len(), expected, "{m:?} length");
            let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
            assert!(peak > 0, "{m:?} must not be silent");
            // ~0.9 full-scale ceiling: no clipping ever.
            assert!(peak <= 30_000, "{m:?} peak {peak} too hot");
        }
    }

    #[test]
    fn synthesize_armor_foley_pcm_is_deterministic_for_fixed_seed() {
        let a = synthesize_armor_foley_pcm(ArmorFoleyMaterial::Iron, 0.8, 48_000, 99);
        let b = synthesize_armor_foley_pcm(ArmorFoleyMaterial::Iron, 0.8, 48_000, 99);
        assert_eq!(a, b, "same seed must reproduce identical PCM");
        let c = synthesize_armor_foley_pcm(ArmorFoleyMaterial::Iron, 0.8, 48_000, 100);
        assert_ne!(a, c, "different seeds should differ");
        // Intensity scales loudness: half intensity <= full-intensity peak.
        let loud = synthesize_armor_foley_pcm(ArmorFoleyMaterial::Iron, 1.0, 48_000, 7);
        let quiet = synthesize_armor_foley_pcm(ArmorFoleyMaterial::Iron, 0.5, 48_000, 7);
        let peak = |p: &[i16]| p.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        assert!(peak(&quiet) < peak(&loud));
    }

    fn foley_runtime(max_volume_cap: f32) -> AudioRuntime {
        AudioRuntime::new(&AudioSessionConfig {
            sink: AudioSinkKind::Null,
            assets_dir: None,
            asset_index: None,
            game_dir: None,
            max_volume_cap,
            max_concurrent_same_sound: 4,
            subtitle_path: None,
            subtitles_to_stderr: false,
        })
    }

    #[test]
    fn trigger_armor_foley_submits_players_channel_voice() {
        let mut rt = foley_runtime(1.0);
        assert!(!rt.trigger_armor_foley(ArmorFoleyMaterial::None, 1.0));
        assert!(!rt.trigger_armor_foley(ArmorFoleyMaterial::Iron, 0.0));
        assert_eq!(rt.stats().events_submitted, 0);

        assert!(rt.trigger_armor_foley(ArmorFoleyMaterial::Iron, 1.0));
        rt.pump_frames(64);
        assert_eq!(rt.stats().events_submitted, 1);
        assert_eq!(rt.mixer.voice_count(), 1, "the foley voice must be live");
        let labels = rt.mixer.voice_labels();
        assert_eq!(labels, ["megu.armor_foley"]);
        // No position + no entity => 2D bypass voice (centered, no reverb).
        let debug = rt.mixer.voice_debug();
        assert_eq!(debug.len(), 1);
        assert!(!debug[0].4, "foley must not spatialize");
        rt.finish();
    }

    #[test]
    fn trigger_armor_foley_respects_zero_volume_cap() {
        let mut rt = foley_runtime(0.0);
        assert!(!rt.trigger_armor_foley(ArmorFoleyMaterial::Netherite, 1.0));
        rt.pump_frames(64);
        assert_eq!(rt.stats().events_submitted, 0);
        rt.finish();
    }

    #[test]
    fn set_environment_hint_reaches_the_mixer() {
        let mut rt = foley_runtime(1.0);
        assert_eq!(rt.mixer.environment_hint(), EnvironmentAudioHint::default());
        rt.set_environment_hint(EnvironmentAudioHint {
            underwater_lowpass: 0.0,
            cave_reverb_mix: 0.6,
            rain_eaves_occlusion: 0.4,
        });
        let hint = rt.mixer.environment_hint();
        assert_eq!(hint.cave_reverb_mix, 0.6);
        assert_eq!(hint.rain_eaves_occlusion, 0.4);
        rt.finish();
    }

    #[test]
    fn stop_sound_cancels_in_flight_decode_but_not_later_ones() {
        let mut rt = decode_test_runtime();
        let (pipe, gate, _calls) = gated_pipeline();
        rt.decode = Some(pipe);

        rt.ingest(&ping_event());
        // Stop arrives while the decode is parked: it must cancel the
        // in-flight job exactly like a queued mixer event.
        rt.ingest_stop(&WireStopSound {
            category: None,
            name: Some("test.ping".to_string()),
        });
        gate.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while rt.decode_outstanding > 0 {
            rt.pump_frames(64);
            assert!(Instant::now() < deadline, "decode never drained");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            rt.stats().events_submitted, 0,
            "stopped decode must never reach the mixer"
        );
        assert_eq!(rt.mixer.voice_count(), 0);

        // A sound ingested *after* the stop is unaffected.
        rt.ingest(&ping_event());
        gate.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            rt.pump_frames(64);
            if rt.stats().events_submitted >= 1 {
                break;
            }
            assert!(Instant::now() < deadline, "post-stop decode never landed");
            std::thread::sleep(Duration::from_millis(5));
        }
        rt.finish();
    }
}
