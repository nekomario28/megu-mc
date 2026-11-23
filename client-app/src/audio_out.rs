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

use std::cell::UnsafeCell;
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::audio_qol::AudioLimiter;
use crate::protocol::{WireSoundEvent, WireStopSound};
use crate::session::{AudioStats, OpenAirWorld, PlaySoundEvent, SoundMixer};
use crate::sound_assets::{
    placeholder_pcm, sound_name_for_id, DecodedPcm, SoundEventRegistry, SoundLibrary,
    MIX_SAMPLE_RATE,
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
    /// Finalize output (patch WAV headers, close the child stdin and reap).
    /// Idempotent; also invoked from `Drop`.
    fn finish(&mut self) {}
}

/// Selected backend from `--audio-sink`/`--audio-wav`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioSinkKind {
    /// Native backend in OS probe order (`pw-cat`/`aplay` on Linux,
    /// CoreAudio on macOS, WinMM on Windows), else `null` (with a warning).
    Auto,
    /// Force `pw-cat`; falls back to `null` with a warning when it cannot
    /// start or dies.
    PipeWire,
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
    /// Linux (and other Unixes that ship `pw-cat`/`aplay`).
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
/// `"alsa"`, `"coreaudio"`, `"winmm"`). Injected into
/// [`detect_available_sinks_for_os`] in production; tests substitute a stub
/// so probe order is checked without spawning or cross-compiling.
///
/// Framework backends (CoreAudio/WinMM) report availability of the
/// *framework* — the queue/device open inside `open_sink` is what decides
/// whether real output exists, and it fails closed to [`NullSink`].
pub fn native_backend_available(key: &str) -> bool {
    match key {
        "pipewire" => path_has_executable("pw-cat"),
        "alsa" => path_has_executable("aplay"),
        "coreaudio" => cfg!(target_os = "macos"),
        "winmm" => cfg!(target_os = "windows"),
        _ => false,
    }
}

/// Ordered native sink candidates for `os`, filtered by `available`.
///
/// - Linux → `[PipeWire, Alsa]` (PipeWire first — the modern stack)
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
const PW_TARGET: [&str; 1] = ["-"];

/// Open one subprocess sink (`pw-cat` or `aplay`); `None` on spawn failure.
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
/// (`pw-cat`→`aplay` on Linux, CoreAudio on macOS, WinMM on Windows) and
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
// AudioRuntime — the live pipeline inside ProtocolClient
// ---------------------------------------------------------------------------

/// The live audio pipeline: wire events in, spatialized/limited PCM out.
/// Created once per connection when `AudioSinkKind::Off` is not selected.
pub struct AudioRuntime {
    /// The deterministic mixer (event queue → voices → limited stereo).
    pub mixer: SoundMixer,
    sink: Box<dyn AudioSink>,
    sink_kind: AudioSinkKind,
    library: Option<SoundLibrary>,
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
    /// Dedicated real-time mix thread once
    /// [`AudioRuntime::enable_dedicated_mix_thread`] hands it the sink —
    /// the Render Thread then drives audio purely through the lock-free
    /// command ring (Gate P3), while `pump`/`pump_frames` stay intact for
    /// the synchronous path.
    mix_thread: Option<AudioMixThread>,
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
            mix_thread: None,
        }
    }

    /// `true` when a runtime should exist for this config.
    pub fn enabled(cfg: &AudioSessionConfig) -> bool {
        cfg.sink != AudioSinkKind::Off
    }

    /// Lazily load the sound library on first use so a session that never
    /// hears a sound never pays the index parse. When `--gameDir` is set,
    /// `mods/*.jar` then `resourcepacks/*` merge on top of the vanilla
    /// index (lowest→highest priority, matching vanilla layer order).
    fn library(&mut self) -> Option<&mut SoundLibrary> {
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
            self.library = lib;
        }
        self.library.as_mut()
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

    /// Ingest one decoded `sound_effect`/`sound_entity` packet.
    pub fn ingest(&mut self, ev: &WireSoundEvent) {
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
        // such. Stereo assets keep their L/R for the 2D-bypass path; the
        // spatializer always consumes the mono downmix.
        let mut pcm = None;
        let mut is_placeholder = false;
        let mut variant_volume = 1.0f32;
        let mut variant_pitch = 1.0f32;
        let mut attenuation_distance = None;
        if let Some(lib) = self.library() {
            match lib.resolve(&name, ev.seed) {
                Some(resolved) => {
                    variant_volume = resolved.volume;
                    variant_pitch = resolved.pitch;
                    attenuation_distance = resolved.attenuation_distance;
                    match lib.decode_source(&resolved.source) {
                        Ok(dpcm) => {
                            let stereo = match &*dpcm {
                                DecodedPcm::Stereo(v) => Some(Arc::new(v.clone())),
                                DecodedPcm::Mono(_) => None,
                            };
                            pcm = Some((Arc::new(dpcm.to_mono()), stereo));
                        }
                        Err(e) => {
                            self.mixer.stats.decode_errors += 1;
                            if self.mixer.stats.decode_errors <= 3 {
                                eprintln!("megu-client audio: {e}");
                            }
                        }
                    }
                }
                None => self.mixer.stats.missing_event_def += 1,
            }
        } else {
            self.mixer.stats.missing_assets += 1;
        }
        let (pcm, stereo_pcm) = match pcm {
            Some((m, s)) => (m, s),
            None => {
                is_placeholder = true;
                self.mixer.stats.placeholder_voices += 1;
                (
                    Arc::new(placeholder_pcm(
                        &name,
                        ev.category,
                        self.mixer.sample_rate(),
                    )),
                    None,
                )
            }
        };
        // Vanilla distance law: the packet volume widens the audible range
        // (`attenuation_distance * max(volume, 1.0)`) while output gain
        // still saturates at `min(volume, 1.0)` inside the mixer — so a
        // `volume=4.0` explosion carries 64 m, not 16. An inline
        // `fixed_range` overrides everything, exactly like the wire
        // semantics.
        let effective_volume = if (ev.volume * variant_volume).is_finite() {
            (ev.volume * variant_volume).max(0.0)
        } else {
            0.0
        };
        let max_range_m = ev.fixed_range.unwrap_or_else(|| {
            attenuation_distance.unwrap_or(16.0) * effective_volume.max(1.0)
        });
        if let Some(eid) = ev.entity_id {
            if self.mixer.entity_position(eid).is_none() {
                self.mixer.stats.entity_pos_unresolved += 1;
            }
        }
        self.mixer.submit(PlaySoundEvent {
            label,
            pcm,
            stereo_pcm,
            is_placeholder,
            category: ev.category,
            position: ev.position,
            entity_id: ev.entity_id,
            volume: effective_volume,
            pitch: (ev.pitch * variant_pitch).clamp(0.5, 2.0),
            seed: ev.seed,
            max_range_m,
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
            self.sink_label(),
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
        // Stop and join the dedicated mix thread first — it owns the real
        // sink once enabled (`self.sink` is a null stand-in by then).
        self.mix_thread = None;
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
            mix_f32: [0.0; AUDIO_MIX_BLOCK_SAMPLES],
            mono_scratch: [0.0; AUDIO_MIX_BLOCK_FRAMES],
            send_scratch: [0.0; AUDIO_MIX_BLOCK_FRAMES],
            stereo_scratch: Vec::with_capacity(AUDIO_MIX_BLOCK_SAMPLES),
            out_i16: [0; AUDIO_MIX_BLOCK_SAMPLES],
            wants_shutdown: false,
        }
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
            } => self.pending.push_back(PendingVoice {
                voice_id,
                pcm,
                pos,
                entity_id,
                volume,
                pitch,
                attenuation_distance,
                is_2d,
            }),
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
            if starved > UNDERRUN_GRACE_BLOCKS {
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
}
