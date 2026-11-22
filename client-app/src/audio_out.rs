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
