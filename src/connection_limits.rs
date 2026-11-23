//! Connection-level limits and abuse guards for the listeners.
//!
//! Everything here is pure policy plus small std-only state so it can be unit
//! tested without a socket. `main.rs` only wires it in. Defaults are chosen so a
//! single developer on loopback never notices them; every limit has an env knob:
//!
//! | env | default | meaning |
//! |---|---|---|
//! | `MEGU_ALLOW_PUBLIC_BIND` | off | required to bind any non-loopback address |
//! | `MEGU_MAX_CONN_PER_IP` | 4 (non-loopback only) | concurrent connections per remote IP, `0` = unlimited; when set explicitly it also applies to loopback |
//! | `MEGU_LOGIN_TIMEOUT_SECS` | 15 | wall-clock budget for handshake + login + configuration |
//! | `MEGU_IDLE_TIMEOUT_SECS` | 60 | play state: no complete packet for this long drops the client |
//! | `MEGU_ALLOWLIST` | unset (off) | comma/space separated player names and/or UUIDs permitted to log in |
//! | `MEGU_RCON_MAX_FAILURES` | 5 | failed RCON passwords from one IP before a lockout |
//! | `MEGU_RCON_LOCKOUT_SECS` | 30 | first lockout length; doubles per further failure, capped at 15 min |
//! | `MEGU_BACKUP_INTERVAL_SECS` | 600 | minimum age before an older world backup generation is rotated |

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Condvar, Mutex, OnceLock,
};
use std::time::{Duration, Instant};

type CachedChunkLookup = (Arc<Vec<u8>>, Vec<((i32, i32, i32), u8)>);

// ----------------------------------------------------------- protocol codecs

/// Login packet 0x03 payload; send before enabling compressed framing.
pub(crate) fn set_compression(threshold: i32) -> Vec<u8> {
    let mut payload = Vec::new();
    crate::write_varint(&mut payload, i64::from(threshold));
    payload
}

/// `MEGU_COMPRESSION_THRESHOLD` resolution: unset means vanilla 1.21.1's
/// 256-byte threshold, `0` or a negative value disables compression, and a
/// positive value is the threshold negotiated with the client.
pub(crate) fn compression_threshold_env(get: impl Fn(&str) -> Option<String>) -> i32 {
    get("MEGU_COMPRESSION_THRESHOLD")
        .and_then(|value| value.parse::<i32>().ok())
        .map_or(256, |value| if value <= 0 { -1 } else { value })
}

/// Zlib level for compressed frames: `MEGU_ZLIB_LEVEL` clamped to
/// `1..=9`, defaulting to 1 (`Compression::fast`). Level 1 is several times
/// quicker than the old level-6 default and the per-chunk send latency win
/// dwarfs the few-percent size cost on already-compressible chunk data.
fn zlib_level_from(value: Option<&str>) -> u32 {
    value
        .and_then(|value| value.parse::<u32>().ok())
        .map_or(1, |level| level.clamp(1, 9))
}

fn zlib_level() -> u32 {
    static LEVEL: OnceLock<u32> = OnceLock::new();
    *LEVEL.get_or_init(|| {
        zlib_level_from(std::env::var("MEGU_ZLIB_LEVEL").ok().as_deref())
    })
}

// ------------------------------------------------------ zlib scratch state

/// Retention cap for a thread's scratch buffers. A pathological packet (one
/// enormous broadcast, say) must not pin its peak output for the life of the
/// thread: anything larger is released on the next call, so the steady-state
/// cost of the pool is a chunk-frame-sized buffer, not the historical peak.
const ZLIB_SCRATCH_RETAIN_CAP: usize = 512 * 1024;

/// One thread's reusable deflate state plus its output buffer.
///
/// Every compressed frame used to build a fresh `Compress` — allocating and
/// initialising the ~256 KiB deflate window/hash tables — and a fresh output
/// `Vec`, per packet, for both chunk-cache misses and broadcast fanout
/// encoding. Level 1 on a 40 KiB chunk produces ~8 KiB, so the tables dwarf
/// the payload and the buffer is worth retaining.
///
/// `miniz_oxide`'s `reset()` still clears its window/hash tables (the same
/// memset a fresh allocation pays), so the win here is structural rather than
/// the memset: no per-packet allocator round-trip, no free, and no first-touch
/// page faults on a fresh 256 KiB block — plus one retained output buffer
/// instead of one allocate/grow/free per frame.
///
/// Output is byte-identical to the `ZlibEncoder` this replaced: `reset()`
/// restores the initial dictionary and Huffman state, so a reused stream and a
/// fresh one emit the same bytes for the same input and level (pinned by
/// `zlib_scratch_matches_a_fresh_encoder_byte_for_byte`).
struct ZlibCompressScratch {
    /// Level the cached stream was built for. The server picks one level per
    /// process (`zlib_level()`), so this only rebuilds under test.
    level: u32,
    deflate: flate2::Compress,
    /// Retained zlib output. Only the bytes written by the most recent
    /// `compress_at` are live: it clears the buffer before deflating.
    out: Vec<u8>,
}

impl ZlibCompressScratch {
    fn new(level: u32) -> Self {
        Self {
            level,
            deflate: flate2::Compress::new(flate2::Compression::new(level), true),
            out: Vec::new(),
        }
    }

    /// Make room for `spare` bytes past the current length without touching
    /// the bytes already there.
    fn reserve_spare(&mut self, spare: usize) {
        let wanted = self.out.len().saturating_add(spare);
        if wanted > self.out.capacity() {
            self.out.reserve_exact(wanted - self.out.len());
        }
    }

    /// Release a buffer a previous spike left oversized.
    fn trim(&mut self) {
        if self.out.capacity() > ZLIB_SCRATCH_RETAIN_CAP {
            self.out = Vec::new();
        }
    }

    /// A complete zlib stream (header, deflate blocks, Adler-32 trailer) for
    /// `input` at `level`, written into the retained buffer.
    fn compress_at(&mut self, level: u32, input: &[u8]) -> &[u8] {
        use flate2::{FlushCompress, Status};

        self.trim();
        if self.level != level {
            self.level = level;
            self.deflate = flate2::Compress::new(flate2::Compression::new(level), true);
        }
        // Reusing the state means resetting it: `total_in`/`total_out` restart
        // at zero and the previous packet leaves nothing behind in the stream.
        self.deflate.reset();
        self.out.clear();
        // Deflate's worst case is a few bytes per 16 KiB block plus a short
        // header, so this covers every packet in a single call.
        self.reserve_spare(input.len() + (input.len() >> 12) + 64);
        let mut fed = 0usize;
        loop {
            if self.out.len() == self.out.capacity() {
                let grow = self.out.capacity().max(1024);
                self.reserve_spare(grow);
            }
            let before = (fed, self.out.len());
            let status = self
                .deflate
                .compress_vec(&input[fed..], &mut self.out, FlushCompress::Finish)
                .expect("deflating into a Vec cannot fail");
            fed = usize::try_from(self.deflate.total_in())
                .unwrap_or(input.len())
                .min(input.len());
            if status == Status::StreamEnd {
                break;
            }
            assert!(
                (fed, self.out.len()) != before,
                "deflate stalled: no input consumed and no output produced"
            );
        }
        debug_assert_eq!(fed, input.len(), "the whole packet was compressed");
        debug_assert_eq!(self.deflate.total_out(), self.out.len() as u64);
        &self.out
    }
}

/// One thread's reusable inflate state plus its output buffer; the receive
/// side of the same trade as `ZlibCompressScratch`.
struct ZlibDecompressScratch {
    inflate: flate2::Decompress,
    out: Vec<u8>,
}

impl ZlibDecompressScratch {
    fn new() -> Self {
        Self {
            inflate: flate2::Decompress::new(true),
            out: Vec::new(),
        }
    }

    fn reserve_spare(&mut self, spare: usize) {
        let wanted = self.out.len().saturating_add(spare);
        if wanted > self.out.capacity() {
            self.out.reserve_exact(wanted - self.out.len());
        }
    }

    fn trim(&mut self) {
        if self.out.capacity() > ZLIB_SCRATCH_RETAIN_CAP {
            self.out = Vec::new();
        }
    }

    /// Inflate `body`, requiring exactly `length` bytes out and every input
    /// byte consumed. Those two checks are the fail-closed guards the caller's
    /// bomb and forged-length tests stand on, so they stay part of the decode
    /// rather than the caller: a stream that decodes longer than it claims, or
    /// leaves trailing bytes, is an error and never a truncation.
    fn inflate_exact(&mut self, body: &[u8], length: usize) -> Result<Vec<u8>, String> {
        use flate2::{FlushDecompress, Status};

        self.trim();
        self.inflate.reset(true);
        self.out.clear();
        // One byte of slack: an over-long stream has somewhere to land and is
        // then caught by the length check instead of being silently cut short.
        self.reserve_spare(length.saturating_add(1));
        let status = self
            .inflate
            .decompress_vec(body, &mut self.out, FlushDecompress::Finish)
            .map_err(|e| format!("invalid zlib data: {e}"))?;
        if status != Status::StreamEnd
            || self.inflate.total_out() != length as u64
            || self.inflate.total_in() != body.len() as u64
        {
            return Err("zlib stream does not match the frame length".into());
        }
        Ok(self.out[..length].to_vec())
    }
}

thread_local! {
    /// Per-thread, not a `Mutex`: frames are compressed on `game_tick` and on
    /// every per-client writer thread, so a shared lock would serialize what
    /// the pool is meant to make cheap.
    static ZLIB_COMPRESS: std::cell::RefCell<ZlibCompressScratch> =
        std::cell::RefCell::new(ZlibCompressScratch::new(zlib_level()));
    static ZLIB_DECOMPRESS: std::cell::RefCell<ZlibDecompressScratch> =
        std::cell::RefCell::new(ZlibDecompressScratch::new());
}

/// Run `f` with this thread's deflate scratch. A nested call (this module
/// never makes one) builds a local state instead of panicking on the borrow.
fn with_zlib_scratch<R>(f: impl FnOnce(&mut ZlibCompressScratch) -> R) -> R {
    ZLIB_COMPRESS.with(|cell| match cell.try_borrow_mut() {
        Ok(mut scratch) => f(&mut scratch),
        Err(_) => f(&mut ZlibCompressScratch::new(zlib_level())),
    })
}

/// The inflate counterpart of `with_zlib_scratch`.
fn with_zlib_inflate<R>(f: impl FnOnce(&mut ZlibDecompressScratch) -> R) -> R {
    ZLIB_DECOMPRESS.with(|cell| match cell.try_borrow_mut() {
        Ok(mut scratch) => f(&mut scratch),
        Err(_) => f(&mut ZlibDecompressScratch::new()),
    })
}

/// Frame a packet (including its packet ID) after compression is enabled.
pub(crate) fn encode_compressed_frame(raw: &[u8], threshold: usize) -> Vec<u8> {
    // One body buffer for both paths; the zlib stream is appended to it from
    // the thread's scratch state, then the outer length is prepended in a
    // single allocation. Every compressed frame on this path — chunk-cache
    // misses (`ChunkPacketCache`), per-broadcast arms (`SharedBroadcastFrame`)
    // and uncached sends (`ChunkPacketCache::frame_uncached`) — lands in the
    // same per-thread deflate state.
    let mut body = Vec::with_capacity(raw.len() + 8);
    if raw.len() < threshold {
        crate::write_varint(&mut body, 0);
        body.extend_from_slice(raw);
    } else {
        crate::write_varint(&mut body, raw.len() as i64);
        let level = zlib_level();
        with_zlib_scratch(|scratch| body.extend_from_slice(scratch.compress_at(level, raw)));
    }
    let mut frame = Vec::with_capacity(body.len() + 5);
    crate::write_varint(&mut frame, body.len() as i64);
    frame.extend_from_slice(&body);
    frame
}

/// Decode the frame body after its outer packet length has been removed.
pub(crate) fn decode_compressed_frame(
    mut body: &[u8],
    threshold: usize,
    max_uncompressed: usize,
) -> Result<Vec<u8>, String> {
    let length = crate::read_protocol_i32(&mut body)
        .and_then(|n| usize::try_from(n).ok())
        .ok_or("invalid data length")?;
    if length == 0 {
        if body.len() >= threshold || body.len() > max_uncompressed {
            return Err("invalid uncompressed packet length".into());
        }
        return Ok(body.to_vec());
    }
    if length < threshold || length > max_uncompressed {
        return Err("invalid compressed packet length".into());
    }
    with_zlib_inflate(|scratch| scratch.inflate_exact(body, length))
}

/// VecDeltaCodec subtracts separately rounded 1/4096-block positions.
/// Use teleport when any component does not fit in a signed short.
pub(crate) fn relative_move_delta(old: [f64; 3], new: [f64; 3]) -> Option<[i16; 3]> {
    let encode = |position: f64| {
        let scaled = position * 4096.0;
        let floor = scaled.floor();
        floor + f64::from(scaled - floor >= 0.5)
    };
    let mut result = [0; 3];
    for i in 0..3 {
        let delta = encode(new[i]) - encode(old[i]);
        if !delta.is_finite() || delta < f64::from(i16::MIN) || delta > f64::from(i16::MAX) {
            return None;
        }
        result[i] = delta as i16;
    }
    Some(result)
}

/// Payload for 0x2e or, with angles, 0x2f. Angles are protocol bytes.
pub(crate) fn entity_relative_move(
    entity_id: i32,
    delta: [i16; 3],
    rotation: Option<(u8, u8)>,
    on_ground: bool,
) -> Vec<u8> {
    let mut payload = Vec::new();
    crate::write_varint(&mut payload, i64::from(entity_id));
    for component in delta {
        payload.extend_from_slice(&component.to_be_bytes());
    }
    if let Some((yaw, pitch)) = rotation {
        payload.extend_from_slice(&[yaw, pitch]);
    }
    payload.push(u8::from(on_ground));
    payload
}

/// Payload for 0x30.
pub(crate) fn entity_rotation(entity_id: i32, yaw: u8, pitch: u8, on_ground: bool) -> Vec<u8> {
    let mut payload = Vec::new();
    crate::write_varint(&mut payload, i64::from(entity_id));
    payload.extend_from_slice(&[yaw, pitch, u8::from(on_ground)]);
    payload
}

/// Payload for 0x48.
pub(crate) fn entity_head_rotation(entity_id: i32, yaw: u8) -> Vec<u8> {
    let mut payload = Vec::new();
    crate::write_varint(&mut payload, i64::from(entity_id));
    payload.push(yaw);
    payload
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // The guarded state is plain counters; a poisoned lock is still usable.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------- chunk delivery

// PlayerChunkSender in Java 1.21.1 clamps feedback to 64 chunks per tick.
const MAX_CHUNKS_PER_TICK: usize = 64;

pub(crate) fn chunk_feedback(window_size: usize) -> crate::ChunkFeedback {
    // Partial clients may wait for the initial view before sending feedback.
    crate::ChunkFeedback::new(window_size.min(MAX_CHUNKS_PER_TICK))
}

pub(crate) fn set_chunk_feedback_window(feedback: &mut crate::ChunkFeedback, size: usize) {
    feedback.set_window(size.min(MAX_CHUNKS_PER_TICK));
}

pub(crate) struct ChunkDelivery {
    budget: crate::ChunkSendBudget,
    window: Option<crate::ChunkWindow>,
    last_batch_tick: Option<u64>,
}

impl Default for ChunkDelivery {
    fn default() -> Self {
        Self {
            budget: crate::ChunkSendBudget::new(MAX_CHUNKS_PER_TICK),
            window: None,
            last_batch_tick: None,
        }
    }
}

impl ChunkDelivery {
    pub(crate) fn window(&self) -> Option<crate::ChunkWindow> {
        self.window
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.budget.queued.is_empty()
    }

    pub(crate) fn reset_window(&mut self) {
        self.window = None;
        self.budget.queued.clear();
    }

    pub(crate) fn next_batch(
        &mut self,
        window: crate::ChunkWindow,
        sent: &HashSet<(i32, i32)>,
        yaw: f32,
        feedback: &crate::ChunkFeedback,
        tick: u64,
    ) -> Vec<(i32, i32)> {
        if self.window != Some(window) {
            self.window = Some(window);
            // Keep the queue bounded even when the ACK gate is closed.
            self.budget.queued.retain(|&chunk| window.contains(chunk));
            for chunk in window.unsent(sent) {
                self.budget.enqueue(chunk);
            }
        }
        if feedback.awaiting() || self.last_batch_tick == Some(tick) || !self.has_pending() {
            return Vec::new();
        }
        self.budget.max_chunks_per_tick = feedback.batch_size();
        let (sin, cos) = f64::from(yaw).rem_euclid(360.0).to_radians().sin_cos();
        // Keep a cardinal side chunk from sorting ahead of a forward chunk.
        let snap = |component: f64| {
            if component.abs() < f64::EPSILON {
                0.0
            } else {
                component
            }
        };
        let batch =
            self.budget
                .drain_tick(window.center, window.radius as u32, (snap(-sin), snap(cos)));
        if !batch.is_empty() {
            self.last_batch_tick = Some(tick);
        }
        batch
    }
}

#[cfg(test)]
mod chunk_delivery_tests {
    use super::*;
    use crate::ChunkWindow;

    #[test]
    fn chunk_delivery_obeys_the_tick_budget_even_after_an_ack() {
        let mut delivery = ChunkDelivery::default();
        let mut feedback = chunk_feedback(289);
        let window = ChunkWindow::at(0.0, 0.0, 8);
        let mut sent = HashSet::new();
        let initial = delivery.next_batch(window, &sent, 0.0, &feedback, 10);
        assert_eq!(initial.len(), 64);
        assert_eq!(initial[0], (0, 0));
        assert!(initial[1..].iter().all(|&(_, z)| z > 0));
        sent.extend(initial);

        feedback.acknowledge(&1000.0f32.to_be_bytes()).unwrap();
        assert!(delivery
            .next_batch(window, &sent, 0.0, &feedback, 10)
            .is_empty());
        // Reverse the look: the remaining chunks are prioritized at drain time.
        let turn = delivery.next_batch(window, &sent, 180.0, &feedback, 11);
        assert_eq!(turn.len(), 64);
        assert!(turn
            .iter()
            .all(|chunk| window.contains(*chunk) && !sent.contains(chunk)));
        assert_eq!(turn[0], (0, -1));
        sent.extend(turn);
        feedback.sent_batch(Instant::now(), 64);
        assert!(delivery
            .next_batch(window, &sent, 180.0, &feedback, 12)
            .is_empty());
        feedback.acknowledge(&2.0f32.to_be_bytes()).unwrap();
        assert_eq!(
            delivery.next_batch(window, &sent, 180.0, &feedback, 12).len(),
            2
        );
    }

    #[test]
    fn chunk_delivery_discards_unsent_chunks_when_the_window_moves() {
        let mut delivery = ChunkDelivery::default();
        let mut feedback = chunk_feedback(25);
        feedback.acknowledge(&1.0f32.to_be_bytes()).unwrap();
        let old = ChunkWindow::at(-32.0, -32.0, 2);
        let mut sent = HashSet::new();
        assert_eq!(
            delivery.next_batch(old, &sent, 0.0, &feedback, 20),
            [(-2, -2)]
        );
        sent.insert((-2, -2));
        assert!(delivery.has_pending());
        feedback.sent_batch(Instant::now(), 1);

        let new = ChunkWindow::at(160.0, 160.0, 1);
        assert_eq!(new.leaving(Some(old), &sent), [(-2, -2)]);
        sent.clear();
        assert!(delivery
            .next_batch(new, &sent, 0.0, &feedback, 21)
            .is_empty());
        feedback.acknowledge(&25.0f32.to_be_bytes()).unwrap();
        let selected = delivery.next_batch(new, &sent, 0.0, &feedback, 21);
        // A d=1 vanilla window tracks 21 chunks (cross shape), not 9.
        assert_eq!(selected.len(), 21);
        assert_eq!(selected[0], (10, 10));
        assert!(selected
            .iter()
            .all(|&chunk| new.contains(chunk) && !old.contains(chunk)));
        assert!(!delivery.has_pending());
        assert!(delivery
            .next_batch(new, &sent, 0.0, &feedback, 22)
            .is_empty());
    }
}

// -------------------------------------------------------- chunk packet frames

/// The single wire form a cached chunk frame retains. Keeping both the plain
/// and the compressed bytes doubled the entry's RAM for no benefit: chunk
/// frames compress ~5:1, so once a compressed form exists the plain buffer is
/// dead weight.
enum CachedPayload {
    /// `varint(len)` + `varint(0x27)` + payload — the exact bytes an
    /// uncompressed client receives.
    Plain(Arc<Vec<u8>>),
    /// `encode_compressed_frame` of the same body, built on first request for
    /// the negotiated threshold (uniform per server: one env knob).
    Compressed { threshold: usize, frame: Arc<Vec<u8>> },
}

/// One fully encoded `0x27` chunk packet, shared by every client that asks for
/// the same column at the same world revision.
struct CachedChunkPacket {
    /// The revision the bytes were produced under. A `frame` call whose
    /// revision differs must miss: the revision folds in the chunk's 3x3 block
    /// versions (bumped by every `set_block*`), its signature (signs, spawners,
    /// neighbour generated emitters, the settled light revision — so a relit
    /// chunk invalidates too), and the live block entities spliced on send.
    revision: u64,
    /// The chunk's own generated light emitters, restored into the caller's
    /// `GenSources` on a hit exactly as a rebuild would record them.
    own_emitters: Vec<((i32, i32, i32), u8)>,
    payload: CachedPayload,
}

/// The `0x27` frame body: packet id + payload.
fn chunk_packet_body(payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(payload.len() + 5);
    crate::write_varint(&mut body, 0x27);
    body.extend_from_slice(payload);
    body
}

/// `body` prefixed by its frame length: the uncompressed wire frame.
fn plain_frame(body: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(body.len() + 5);
    crate::write_varint(&mut frame, body.len() as i64);
    frame.extend_from_slice(body);
    frame
}

/// The body inside a `plain_frame` (the frames this cache builds; `plain` is
/// `varint(len)` + body).
fn body_of_frame(plain: &[u8]) -> &[u8] {
    let mut rest = plain;
    let _ = crate::read_protocol_i32(&mut rest);
    rest
}

/// Generation-keyed, revision-gated cache of `0x27` chunk packet frames.
///
/// Streaming one column to N players used to rebuild the packet per reader:
/// `map_chunk` (itself payload-cached), the live block-entity splice, the light
/// cull — and, when compression is on, a zlib pass per client per chunk, the
/// most expensive step of the send. The bytes depend only on the world state
/// the revision folds together, so every reader at the same revision receives
/// literally the same `Arc`, and a player re-entering a column reuses it too.
/// Entries are bounded (the same 512 the payload/light caches use) and dropped
/// wholesale when the generator's `generation` moves, like `MapChunkCache`.
///
/// The key's last field is the anti-xray occlusion lane: `false` is the real
/// chunk (underground players, spectators), `true` the surface lane whose deep
/// sections are masked to solid host rock. Two lanes for one column never
/// share a frame.
pub(crate) struct ChunkPacketCache {
    generation: u64,
    map: HashMap<(crate::DimId, i32, i32, bool), CachedChunkPacket>,
    order: VecDeque<(crate::DimId, i32, i32, bool)>,
}

/// Entries retained; each holds one wire form of a full chunk frame — the
/// plain frame, or only its compressed form once a compressed client asks.
const CHUNK_PACKET_CACHE_CAP: usize = 512;

impl ChunkPacketCache {
    pub(crate) fn new() -> Self {
        Self {
            generation: 0,
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn sync_generation(&mut self, generation: u64) {
        if self.generation != generation {
            self.map.clear();
            self.order.clear();
            self.generation = generation;
        }
    }

    /// The ready-to-write frame for `key` at `revision`, with the chunk's own
    /// emitters to restore into the caller's `GenSources`. `None` when the
    /// entry is absent or stale — the caller rebuilds and offers `insert`.
    pub(crate) fn frame(
        &mut self,
        generation: u64,
        key: (crate::DimId, i32, i32, bool),
        revision: u64,
        compression: Option<usize>,
    ) -> Option<CachedChunkLookup> {
        self.sync_generation(generation);
        let entry = self.map.get_mut(&key)?;
        if entry.revision != revision {
            return None;
        }
        let frame = match compression {
            None => match &entry.payload {
                CachedPayload::Plain(frame) => frame.clone(),
                // The plain buffer was dropped on the first compressed
                // request; a mode change is a miss and the caller rebuilds.
                CachedPayload::Compressed { .. } => return None,
            },
            Some(threshold) => match &entry.payload {
                CachedPayload::Compressed {
                    threshold: stored,
                    frame,
                } if *stored == threshold => frame.clone(),
                // A different threshold needs the plain bytes, which are
                // gone: miss so the caller rebuilds and re-inserts.
                CachedPayload::Compressed { .. } => return None,
                CachedPayload::Plain(plain) => {
                    // Compress once, then swap the payload: assigning drops
                    // the ~20-40 KiB plain buffer in the same move.
                    let frame = Arc::new(encode_compressed_frame(
                        body_of_frame(plain),
                        threshold,
                    ));
                    entry.payload = CachedPayload::Compressed {
                        threshold,
                        frame: frame.clone(),
                    };
                    frame
                }
            },
        };
        Some((frame, entry.own_emitters.clone()))
    }

    /// Approximate heap bytes retained: each entry's frame buffer at its
    /// allocated capacity, its emitter list, and the entry struct itself.
    /// Measures the plain-drop win directly in tests and diagnostics.
    #[allow(dead_code)]
    pub(crate) fn estimated_bytes(&self) -> usize {
        self.map
            .values()
            .map(|entry| {
                let frame_bytes = match &entry.payload {
                    CachedPayload::Plain(frame)
                    | CachedPayload::Compressed { frame, .. } => frame.capacity(),
                };
                frame_bytes
                    + entry.own_emitters.capacity()
                        * std::mem::size_of::<((i32, i32, i32), u8)>()
                    + std::mem::size_of::<CachedChunkPacket>()
            })
            .sum()
    }

    /// Store `payload` (no packet id — the `0x27` prefix is added here) at
    /// `revision`. Only call when the revision's inputs held for the whole
    /// build; a mid-build edit must be a miss on the next `frame`, not a stale
    /// hit under the old revision.
    pub(crate) fn insert(
        &mut self,
        generation: u64,
        key: (crate::DimId, i32, i32, bool),
        revision: u64,
        own_emitters: Vec<((i32, i32, i32), u8)>,
        payload: Vec<u8>,
    ) {
        self.sync_generation(generation);
        let entry = CachedChunkPacket {
            revision,
            own_emitters,
            payload: CachedPayload::Plain(Arc::new(plain_frame(&chunk_packet_body(&payload)))),
        };
        if self.map.insert(key, entry).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > CHUNK_PACKET_CACHE_CAP {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }

    /// The frame for a payload that must not be retained (the build raced an
    /// edit). The bytes sent are identical to a cached entry's.
    pub(crate) fn frame_uncached(payload: &[u8], compression: Option<usize>) -> Arc<Vec<u8>> {
        let body = chunk_packet_body(payload);
        match compression {
            None => Arc::new(plain_frame(&body)),
            Some(threshold) => Arc::new(encode_compressed_frame(&body, threshold)),
        }
    }

    #[cfg(test)]
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    pub(crate) fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }
}

// ------------------------------------------------------- per-tick coalescing

/// Batches the small S2C frames one tick emits into a single `write_all`,
/// flushing early only when the buffer would exceed `capacity` (default
/// 64 KiB). Paired with `TCP_NODELAY` the bytes still leave at the end-of-tick
/// `flush`, so delivery latency is identical while a burst of small packets
/// costs one syscall instead of one per packet.
pub(crate) struct PacketCoalescer {
    buf: Vec<u8>,
    capacity: usize,
}

impl PacketCoalescer {
    /// Default auto-flush threshold.
    pub(crate) const DEFAULT_CAPACITY: usize = 64 * 1024;

    pub(crate) fn new() -> Self {
        Self::with_capacity(Self::DEFAULT_CAPACITY)
    }

    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: Vec::new(),
            capacity: capacity.max(1),
        }
    }

    /// Bytes queued for the next `flush`.
    #[cfg(test)]
    pub(crate) fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Queue `frame` for the next flush. Flushes first when appending would
    /// exceed the capacity; a frame larger than the whole buffer is written
    /// through directly so the buffer stays bounded.
    pub(crate) fn push_frame(
        &mut self,
        frame: &[u8],
        w: &mut impl std::io::Write,
    ) -> std::io::Result<()> {
        if self.buf.len() + frame.len() > self.capacity {
            self.flush(w)?;
            if frame.len() >= self.capacity {
                return w.write_all(frame);
            }
        }
        self.buf.extend_from_slice(frame);
        if self.buf.len() >= self.capacity {
            self.flush(w)?;
        }
        Ok(())
    }

    /// Write everything queued so far as one `write_all`.
    pub(crate) fn flush(&mut self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        w.write_all(&self.buf)?;
        self.buf.clear();
        Ok(())
    }
}

// ----------------------------------- shared broadcast frames + tx queues

/// One S2C packet framed once and fanned out to every broadcast recipient.
///
/// Broadcasting used to pay the framing — and with compression on, a zlib
/// pass — once per player, so a 50-client broadcast compressed identical
/// bytes 50 times. The packet body (`varint(packet_id)` + payload) is built
/// once up front; each wire form is then produced lazily on the first
/// `frame_for` that asks for it and shared as an `Arc<[u8]>` clone
/// afterwards, so the fanout encodes once and hands out refcount bumps. The
/// compressed slot is keyed by its threshold: the server negotiates a single
/// threshold for every compressed client, and a different request rebuilds
/// (counting as another encode) rather than retaining both forms.
pub(crate) struct SharedBroadcastFrame {
    /// `varint(packet_id)` + payload — what `encode_compressed_frame`
    /// compresses and `plain_frame` prefixes. Immutable once built.
    body: Box<[u8]>,
    /// Lazily built uncompressed frame (`varint(len)` + body).
    plain: Option<Arc<[u8]>>,
    /// Lazily built compressed frame and the threshold it was built at.
    compressed: Option<(usize, Arc<[u8]>)>,
    /// Wire-form builds so far — the metric the encode-once claim stands on.
    encode_count: usize,
}

impl SharedBroadcastFrame {
    pub(crate) fn new(packet_id: i64, payload: &[u8]) -> Self {
        let mut body = Vec::with_capacity(payload.len() + 5);
        crate::write_varint(&mut body, packet_id);
        body.extend_from_slice(payload);
        Self {
            body: body.into_boxed_slice(),
            plain: None,
            compressed: None,
            encode_count: 0,
        }
    }

    /// The wire frame for `compression`, built on the first request and
    /// shared as the same `Arc` on every later one.
    pub(crate) fn frame_for(&mut self, compression: Option<usize>) -> Arc<[u8]> {
        match compression {
            None => {
                let body = &self.body;
                let encode_count = &mut self.encode_count;
                self.plain
                    .get_or_insert_with(|| {
                        *encode_count += 1;
                        Arc::from(plain_frame(body))
                    })
                    .clone()
            }
            Some(threshold) => {
                let rebuild = self
                    .compressed
                    .as_ref()
                    .is_none_or(|(stored, _)| *stored != threshold);
                if rebuild {
                    let frame: Arc<[u8]> =
                        Arc::from(encode_compressed_frame(&self.body, threshold));
                    self.compressed = Some((threshold, frame));
                    self.encode_count += 1;
                }
                match &self.compressed {
                    Some((_, frame)) => frame.clone(),
                    None => unreachable!("the frame was built above"),
                }
            }
        }
    }

    /// How many wire forms were actually built. A broadcast that served every
    /// client at one compression mode keeps this at 1.
    #[cfg(test)]
    pub(crate) fn encode_count(&self) -> usize {
        self.encode_count
    }
}

/// Why `ClientTxQueue::try_enqueue` refused a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TxQueueError {
    /// The byte budget is full: this client's TCP send buffer is not
    /// draining. Dropping the slow client costs one player; blocking
    /// `game_tick` on its socket would stall all of them.
    BackpressureSlowClient,
    /// `close` already ran — no new frames are accepted.
    Closed,
}

/// A strictly bounded, never-blocking outbound queue for one client.
///
/// `game_tick` must never wait on a socket: one client whose TCP send buffer
/// is full would otherwise hold the whole broadcast loop head-of-line while
/// every other player waits. Producers `try_enqueue` `Arc<[u8]>` frames —
/// the same allocations a `SharedBroadcastFrame` hands out, so a broadcast
/// queueing onto 50 clients is 50 refcount bumps and zero copies — in O(1)
/// and never block on I/O; over the byte budget the call fails fast with
/// `TxQueueError::BackpressureSlowClient` so the caller disconnects only the
/// slow client. A dedicated per-client thread runs `writer_loop`: the
/// `Condvar` wakes it on the empty→non-empty transition, it drains the batch,
/// feeds every frame through `PacketCoalescer`, and flushes once per batch.
/// `close` wakes it one last time so it drains and exits — no leaked thread.
pub(crate) struct ClientTxQueue {
    inner: Mutex<TxQueueInner>,
    /// Signalled on the empty→non-empty transition and by `close`.
    wake: Condvar,
    max_queued_bytes: usize,
}

#[derive(Default)]
struct TxQueueInner {
    queue: VecDeque<Arc<[u8]>>,
    /// Sum of `frame.len()` over `queue` — the bound `try_enqueue` enforces.
    queued_bytes: usize,
    closed: bool,
}

impl ClientTxQueue {
    /// The default per-client budget: 4 MiB of outbound frames. Generous
    /// enough that a healthy client never sees it; small enough that a
    /// dead socket is cut before it becomes a memory problem.
    pub(crate) const DEFAULT_MAX_QUEUED_BYTES: usize = 4 * 1024 * 1024;

    pub(crate) fn new(max_queued_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(TxQueueInner::default()),
            wake: Condvar::new(),
            max_queued_bytes,
        }
    }

    /// Queue `frame` for the writer thread without ever blocking. Fails fast
    /// — rather than stalling the caller — when the byte budget would be
    /// exceeded or the queue is closed.
    pub(crate) fn try_enqueue(&self, frame: Arc<[u8]>) -> Result<(), TxQueueError> {
        let mut inner = lock(&self.inner);
        if inner.closed {
            return Err(TxQueueError::Closed);
        }
        if inner.queued_bytes.saturating_add(frame.len()) > self.max_queued_bytes {
            return Err(TxQueueError::BackpressureSlowClient);
        }
        // Notify only on the empty→non-empty transition: the writer sleeps
        // only while the queue is empty, and a non-empty queue is drained by
        // the batch it is already about to take.
        let wake_writer = inner.queue.is_empty();
        inner.queued_bytes += frame.len();
        inner.queue.push_back(frame);
        drop(inner);
        if wake_writer {
            self.wake.notify_one();
        }
        Ok(())
    }

    /// Move every currently queued frame into `out` in FIFO order and zero
    /// the byte count. `writer_loop` calls this once per woken batch.
    pub(crate) fn drain_batch(&self, out: &mut Vec<Arc<[u8]>>) {
        let mut inner = lock(&self.inner);
        inner.queued_bytes = 0;
        out.extend(inner.queue.drain(..));
    }

    /// Mark the queue closed and wake the writer so `writer_loop` can drain
    /// what remains and exit. Frames already queued are still written.
    pub(crate) fn close(&self) {
        lock(&self.inner).closed = true;
        self.wake.notify_all();
    }

    /// Bytes currently queued — the budget `try_enqueue` enforces.
    #[cfg(test)]
    pub(crate) fn queued_bytes(&self) -> usize {
        lock(&self.inner).queued_bytes
    }

    /// Block until a frame is queued or the queue is closed. `false` means
    /// closed and drained: the writer exits.
    fn wait_for_work(&self) -> bool {
        let mut inner = lock(&self.inner);
        while inner.queue.is_empty() && !inner.closed {
            inner = self.wake.wait(inner).unwrap_or_else(|e| e.into_inner());
        }
        !(inner.closed && inner.queue.is_empty())
    }

    /// The dedicated per-client writer loop: sleep on the `Condvar` until a
    /// batch is ready, push every drained frame through `PacketCoalescer`,
    /// and flush once per batch — a burst of small packets costs one
    /// `write_all`, not one per packet. Returns when `close` ran and the
    /// backlog drained, or early on a write error (the caller then drops the
    /// client).
    pub(crate) fn writer_loop(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        let mut coalescer = PacketCoalescer::new();
        let mut batch = Vec::new();
        while self.wait_for_work() {
            self.drain_batch(&mut batch);
            for frame in batch.drain(..) {
                coalescer.push_frame(&frame, w)?;
            }
            coalescer.flush(w)?;
        }
        coalescer.flush(w)
    }
}

impl Default for ClientTxQueue {
    fn default() -> Self {
        Self::new(Self::DEFAULT_MAX_QUEUED_BYTES)
    }
}

// --------------------------------------------------- broadcast range policy

/// The radius vanilla `PlayerList.broadcast(except, x, y, z, radius, dimension,
/// packet)` uses for a positioned world-effect packet, read from the packet
/// itself: sounds (`0x68`) carry `16 * volume` with a 16-block floor, level
/// particles (`0x29`) reach 512 blocks only under their long-distance flag
/// (otherwise 32), and explosions (`0x20`) reach 64. `None` means the packet
/// has no vanilla range contract — a culled broadcast must refuse to send it
/// rather than silently reach everyone.
pub(crate) fn broadcast_radius(id: i64, payload: &[u8]) -> Option<f64> {
    match id {
        0x68 => {
            // named sound: varint id+1, varint source, fixed-point x/y/z (x8
            // i32), f32 volume — the range check needs only the volume.
            let mut body = payload;
            crate::read_protocol_i32(&mut body)?;
            crate::read_protocol_i32(&mut body)?;
            let volume = f32::from_be_bytes(body.get(12..16)?.try_into().ok()?);
            Some((16.0 * f64::from(volume)).max(16.0))
        }
        0x29 => Some(if payload.first() == Some(&1) { 512.0 } else { 32.0 }),
        0x20 => Some(64.0),
        _ => None,
    }
}

/// `PlayerList.broadcast` sends strictly inside the radius (`d² < r²`), in the
/// same dimension (the caller checks that separately).
pub(crate) fn within_broadcast_range(
    px: f64,
    py: f64,
    pz: f64,
    x: f64,
    y: f64,
    z: f64,
    radius: f64,
) -> bool {
    let (dx, dy, dz) = (px - x, py - y, pz - z);
    dx * dx + dy * dy + dz * dz < radius * radius
}

// ---------------------------------------------------------- play keepalive

/// Vanilla 1.21.1 sends a keepalive every 15 seconds and expects the matching
/// response within the same interval. This state is owned by one connection.
pub(crate) const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeepaliveAction {
    Wait,
    Send(i64),
    TimedOut,
}

#[derive(Debug)]
pub(crate) struct KeepaliveState {
    last_sent: Instant,
    pending: Option<i64>,
}

impl KeepaliveState {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            last_sent: now,
            pending: None,
        }
    }

    /// `id` is used only when the next challenge is due.
    pub(crate) fn tick(&mut self, now: Instant, id: i64) -> KeepaliveAction {
        if now.saturating_duration_since(self.last_sent) < KEEPALIVE_INTERVAL {
            return KeepaliveAction::Wait;
        }
        if self.pending.is_some() {
            return KeepaliveAction::TimedOut;
        }
        self.last_sent = now;
        self.pending = Some(id);
        KeepaliveAction::Send(id)
    }

    pub(crate) fn reply(&mut self, now: Instant, payload: &[u8]) -> bool {
        let Ok(bytes) = <[u8; 8]>::try_from(payload) else {
            return false;
        };
        if now.saturating_duration_since(self.last_sent) >= KEEPALIVE_INTERVAL
            || self.pending != Some(i64::from_be_bytes(bytes))
        {
            return false;
        }
        self.pending = None;
        true
    }
}

#[cfg(test)]
mod keepalive_tests {
    use super::*;

    #[test]
    fn keepalive_waits_and_accepts_the_exact_outstanding_id() {
        let start = Instant::now();
        let sent = start + KEEPALIVE_INTERVAL;
        let mut state = KeepaliveState::new(start);
        assert_eq!(state.tick(start, -9), KeepaliveAction::Wait);
        assert_eq!(state.tick(sent, -9), KeepaliveAction::Send(-9));
        let reply = sent + Duration::from_secs(1);
        assert_eq!(state.tick(reply, 4), KeepaliveAction::Wait);
        assert!(!state.reply(reply, &4i64.to_be_bytes()));
        assert!(!state.reply(reply, &(-9i64).to_be_bytes()[..7]));
        assert!(state.reply(reply, &(-9i64).to_be_bytes()));
        assert_eq!(
            state.tick(sent + KEEPALIVE_INTERVAL, 4),
            KeepaliveAction::Send(4)
        );
    }

    #[test]
    fn challenge_deadline_expires_despite_other_traffic() {
        let start = Instant::now();
        let sent = start + KEEPALIVE_INTERVAL;
        let mut state = KeepaliveState::new(start);
        assert_eq!(state.tick(sent, 1), KeepaliveAction::Send(1));
        for second in 1..15 {
            assert_eq!(
                state.tick(sent + Duration::from_secs(second), 2),
                KeepaliveAction::Wait
            );
        }
        let deadline = sent + KEEPALIVE_INTERVAL;
        assert!(!state.reply(deadline, &1i64.to_be_bytes()));
        assert_eq!(state.tick(deadline, 2), KeepaliveAction::TimedOut);
    }
}

#[derive(Default, PartialEq, Eq)]
enum TeleportPhase {
    #[default]
    Idle,
    Preparing,
    Sent,
}

#[derive(Default)]
pub(crate) struct TeleportSync {
    id: i32,
    phase: TeleportPhase,
}

impl TeleportSync {
    pub(crate) fn begin(&mut self) -> i32 {
        self.id = (self.id + 1) % i32::MAX;
        self.phase = TeleportPhase::Preparing;
        self.id
    }

    pub(crate) fn preparing(&self, id: i32) -> bool {
        self.phase == TeleportPhase::Preparing && self.id == id
    }

    pub(crate) fn sent(&mut self) {
        self.phase = TeleportPhase::Sent;
    }

    pub(crate) fn confirm(&mut self, id: i32) {
        if self.phase == TeleportPhase::Sent && self.id == id {
            self.phase = TeleportPhase::Idle;
        }
    }

    pub(crate) fn movement_allowed(&self) -> bool {
        self.phase == TeleportPhase::Idle
    }
}

#[cfg(test)]
#[test]
fn teleport_only_accepts_the_current_sent_id() {
    let mut sync = TeleportSync::default();
    assert!(sync.movement_allowed());
    let first = sync.begin();
    sync.sent();
    let current = sync.begin();
    assert_ne!(first, current);
    assert!(sync.preparing(current));
    sync.confirm(current);
    assert!(!sync.movement_allowed());
    sync.sent();
    sync.confirm(first);
    assert!(!sync.movement_allowed());
    sync.confirm(current);
    assert!(sync.movement_allowed());
    sync.id = i32::MAX - 1;
    assert_eq!(sync.begin(), 0);
}

fn env_parse<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

// ---------------------------------------------------------------- bind policy

/// Loopback means a client on this host. `localhost` is accepted by name; any
/// other hostname or address (including `0.0.0.0` and `::`) is not loopback.
pub(crate) fn is_loopback_bind(addr: &str) -> bool {
    let a = addr.trim().trim_start_matches('[').trim_end_matches(']');
    if a.eq_ignore_ascii_case("localhost") {
        return true;
    }
    a.parse::<IpAddr>()
        .is_ok_and(|ip| ip.to_canonical().is_loopback())
}

pub(crate) fn public_bind_allowed() -> bool {
    std::env::var("MEGU_ALLOW_PUBLIC_BIND").is_ok_and(|v| truthy(&v))
}

/// `Ok` for loopback, or for anything when the operator opted in explicitly.
pub(crate) fn check_bind(what: &str, addr: &str, allow_public: bool) -> Result<(), String> {
    if allow_public || is_loopback_bind(addr) {
        return Ok(());
    }
    Err(format!(
        "refusing to bind {what} to non-loopback address {addr}: this server uses offline-mode \
         identities (no authentication) and plaintext traffic. Set MEGU_ALLOW_PUBLIC_BIND=1 \
         only behind a firewall/private network you control."
    ))
}

// ------------------------------------------------------------ timeouts / knobs

pub(crate) fn login_timeout() -> Duration {
    Duration::from_secs(
        env_parse("MEGU_LOGIN_TIMEOUT_SECS")
            .unwrap_or(15)
            .max(1),
    )
}

static IDLE_TIMEOUT_SECS: OnceLock<AtomicU64> = OnceLock::new();

fn idle_timeout_secs() -> &'static AtomicU64 {
    IDLE_TIMEOUT_SECS.get_or_init(|| {
        AtomicU64::new(env_parse("MEGU_IDLE_TIMEOUT_SECS").unwrap_or(60).max(1))
    })
}

pub(crate) fn idle_timeout() -> Duration {
    Duration::from_secs(idle_timeout_secs().load(Ordering::Relaxed))
}

/// Set the play-state idle timeout. Zero disables idle disconnection.
pub(crate) fn set_idle_timeout(timeout: Duration) {
    idle_timeout_secs().store(timeout.as_secs(), Ordering::Relaxed);
}

pub(crate) fn backup_interval() -> Duration {
    Duration::from_secs(env_parse("MEGU_BACKUP_INTERVAL_SECS").unwrap_or(600))
}

// ------------------------------------------------------------ per-IP limiting

/// Concurrent connection count per remote address.
pub(crate) struct IpConnections {
    counts: Mutex<BTreeMap<IpAddr, usize>>,
}

/// Releases the per-IP slot on drop, including when the handler panics.
pub(crate) struct IpSlot {
    table: &'static IpConnections,
    ip: IpAddr,
}

impl Drop for IpSlot {
    fn drop(&mut self) {
        let mut c = lock(&self.table.counts);
        if let Some(n) = c.get_mut(&self.ip) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                c.remove(&self.ip);
            }
        }
    }
}

impl IpConnections {
    pub(crate) const fn new() -> Self {
        IpConnections {
            counts: Mutex::new(BTreeMap::new()),
        }
    }

    /// `limit == 0` means unlimited (the slot is still counted). Returns `None`
    /// when `ip` already holds `limit` connections.
    pub(crate) fn acquire(&'static self, ip: IpAddr, limit: usize) -> Option<IpSlot> {
        let ip = ip.to_canonical();
        let mut c = lock(&self.counts);
        let n = c.entry(ip).or_insert(0);
        if limit != 0 && *n >= limit {
            if *n == 0 {
                c.remove(&ip);
            }
            return None;
        }
        *n += 1;
        Some(IpSlot { table: self, ip })
    }

    #[cfg(test)]
    fn count(&self, ip: IpAddr) -> usize {
        lock(&self.counts)
            .get(&ip.to_canonical())
            .copied()
            .unwrap_or(0)
    }
}

/// Per-IP limit for `ip`. Default 4, not applied to loopback (a developer's own
/// bots, or a local reverse proxy where everyone is 127.0.0.1); setting
/// `MEGU_MAX_CONN_PER_IP` explicitly applies it everywhere. `0` = unlimited.
pub(crate) fn per_ip_limit(ip: IpAddr) -> usize {
    per_ip_limit_with(ip, env_parse::<usize>("MEGU_MAX_CONN_PER_IP"))
}

pub(crate) fn per_ip_limit_with(ip: IpAddr, explicit: Option<usize>) -> usize {
    match explicit {
        Some(n) => n,
        None if ip.to_canonical().is_loopback() => 0,
        None => 4,
    }
}

// ------------------------------------------------------- duplicate login guard

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LoginRefusal {
    NameInUse,
    UuidInUse,
}

/// Identities that are logged in or still finishing logout. A reservation is
/// taken atomically at login and held until the connection handler has finished
/// saving the player, so a second login with the same name cannot load stale
/// inventory/position while the first session is still writing them.
pub(crate) struct LoginRegistry {
    held: Mutex<Vec<(String, [u8; 16])>>,
}

pub(crate) struct LoginGuard {
    registry: &'static LoginRegistry,
    name: String,
    uuid: [u8; 16],
}

impl Drop for LoginGuard {
    fn drop(&mut self) {
        let mut h = lock(&self.registry.held);
        if let Some(i) = h
            .iter()
            .position(|(n, u)| *n == self.name && *u == self.uuid)
        {
            h.swap_remove(i);
        }
    }
}

impl LoginRegistry {
    pub(crate) const fn new() -> Self {
        LoginRegistry {
            held: Mutex::new(Vec::new()),
        }
    }

    /// Names compare case-insensitively (as vanilla does); UUIDs exactly.
    pub(crate) fn reserve(
        &'static self,
        name: &str,
        uuid: [u8; 16],
    ) -> Result<LoginGuard, LoginRefusal> {
        let key = name.to_ascii_lowercase();
        let mut h = lock(&self.held);
        if h.iter().any(|(n, _)| *n == key) {
            return Err(LoginRefusal::NameInUse);
        }
        if h.iter().any(|(_, u)| *u == uuid) {
            return Err(LoginRefusal::UuidInUse);
        }
        h.push((key.clone(), uuid));
        Ok(LoginGuard {
            registry: self,
            name: key,
            uuid,
        })
    }
}

// ------------------------------------------------------------------ allowlist

/// Optional login allowlist of names and/or UUIDs. `None` from `parse` means
/// the feature is off.
#[derive(Debug, Default)]
pub(crate) struct Allowlist {
    names: HashSet<String>,
    uuids: HashSet<[u8; 16]>,
}

fn parse_uuid(s: &str) -> Option<[u8; 16]> {
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

impl Allowlist {
    pub(crate) fn parse(spec: &str) -> Option<Allowlist> {
        let mut list = Allowlist::default();
        for tok in spec.split(|c: char| c == ',' || c.is_whitespace()) {
            if tok.is_empty() {
                continue;
            }
            match parse_uuid(tok) {
                Some(u) => {
                    list.uuids.insert(u);
                }
                None => {
                    list.names.insert(tok.to_ascii_lowercase());
                }
            }
        }
        (!list.names.is_empty() || !list.uuids.is_empty()).then_some(list)
    }

    pub(crate) fn permits(&self, name: &str, uuid: &[u8; 16]) -> bool {
        self.names.contains(&name.to_ascii_lowercase()) || self.uuids.contains(uuid)
    }
}

/// The configured allowlist (`MEGU_ALLOWLIST`), read once. `None` = off.
pub(crate) fn allowlist() -> Option<&'static Allowlist> {
    static LIST: std::sync::OnceLock<Option<Allowlist>> = std::sync::OnceLock::new();
    LIST.get_or_init(|| {
        let list = Allowlist::parse(&std::env::var("MEGU_ALLOWLIST").unwrap_or_default());
        if let Some(l) = &list {
            eprintln!(
                "[security] login allowlist active: {} name(s), {} uuid(s)",
                l.names.len(),
                l.uuids.len()
            );
        }
        list
    })
    .as_ref()
}

// ----------------------------------------------------------------------- chat

/// Vanilla's `ServerboundChatPacket` / `ServerboundChatCommandPacket` bound.
pub(crate) const MAX_CHAT_CHARS: usize = 256;

/// Vanilla `SharedConstants.isAllowedChatCharacter`: no section sign, no
/// control characters. A vanilla client cannot type these, so a message that
/// has them is not from one; vanilla disconnects for it.
pub(crate) fn chat_text_valid(s: &str) -> bool {
    s.chars().count() <= MAX_CHAT_CHARS
        && s.chars()
            .all(|c| c != '\u{a7}' && c >= ' ' && c != '\u{7f}')
}

/// Vanilla's chat spam counter (`ServerGamePacketListenerImpl.chatSpamThreshold`):
/// every chat message or command adds 20, the counter drains one point per
/// 50 ms tick, and exceeding 200 disconnects ("Kicked for spamming!"). That is
/// a burst of 10 and a sustained one message per second.
pub(crate) struct ChatThrottle {
    score: u64,
    last: Instant,
}

const SPAM_COST: u64 = 20;
const SPAM_LIMIT: u64 = 200;
const SPAM_DRAIN_MS: u64 = 50;

impl ChatThrottle {
    pub(crate) fn new(now: Instant) -> Self {
        ChatThrottle {
            score: 0,
            last: now,
        }
    }

    /// `false` means the player exceeded the limit and should be kicked.
    pub(crate) fn allow(&mut self, now: Instant) -> bool {
        let drained = now.saturating_duration_since(self.last).as_millis() as u64 / SPAM_DRAIN_MS;
        if drained > 0 {
            self.score = self.score.saturating_sub(drained);
            self.last += Duration::from_millis(drained * SPAM_DRAIN_MS);
        }
        self.score += SPAM_COST;
        self.score <= SPAM_LIMIT
    }
}

/// Per-play-connection packet budget. The 1,200-packet burst admits several
/// ticks of teleport/chunk-ack responses and creative inventory operations;
/// 500 packets/s replenishment is well above normal movement (20-60/s).
/// Sustained overage is tracked independently of occasional admitted packets,
/// so a flood cannot avoid disconnection by mixing accepted and dropped frames.
pub(crate) struct PacketRateLimit {
    tokens: f64,
    last: Instant,
    over_limit_since: Option<Instant>,
    last_denied: Option<Instant>,
    dropped: u64,
}

impl PacketRateLimit {
    const CAPACITY: f64 = 1200.0;
    const REFILL_PER_SEC: f64 = 500.0;
    const OVER_LIMIT_WINDOW: Duration = Duration::from_secs(2);
    const DENIAL_GAP: Duration = Duration::from_millis(500);

    pub(crate) fn new(now: Instant) -> Self {
        Self {
            tokens: Self::CAPACITY,
            last: now,
            over_limit_since: None,
            last_denied: None,
            dropped: 0,
        }
    }

    /// Debit one decoded play packet. Denied frames cost no CPU-heavy dispatch
    /// or broadcast work, while real elapsed time always restores capacity.
    pub(crate) fn allow(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * Self::REFILL_PER_SEC).min(Self::CAPACITY);
        self.last = now;
        if self.last_denied.is_some_and(|last| now.saturating_duration_since(last) > Self::DENIAL_GAP) {
            self.over_limit_since = None;
            self.last_denied = None;
        }
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            self.dropped = self.dropped.saturating_add(1);
            self.over_limit_since.get_or_insert(now);
            self.last_denied = Some(now);
            false
        }
    }

    /// Disconnect only after two seconds of continually rejected traffic.
    /// A quiet client recovers the full burst budget without a wall-clock wait.
    pub(crate) fn should_disconnect(&self, now: Instant) -> bool {
        self.over_limit_since
            .is_some_and(|start| now.saturating_duration_since(start) >= Self::OVER_LIMIT_WINDOW)
    }

    pub(crate) fn dropped(&self) -> u64 {
        self.dropped
    }
}

// ----------------------------------------------------------------------- RCON

/// Constant-time byte comparison; also hides which byte differs, and compares
/// over the longer of the two inputs so a short guess is not faster to reject.
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let n = a.len().max(b.len());
    let mut diff = u8::from(a.len() != b.len());
    for i in 0..n {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    std::hint::black_box(diff) == 0
}

struct AuthEntry {
    failures: u32,
    locked_until: Option<Instant>,
    last_failure: Instant,
}

/// Per-IP failed-password throttle: after `max_failures` failures the address
/// is locked out for `base_lockout`, doubling for each further failure up to
/// `MAX_LOCKOUT`. A success clears the address.
pub(crate) struct AuthThrottle {
    entries: Mutex<BTreeMap<IpAddr, AuthEntry>>,
}

const MAX_LOCKOUT: Duration = Duration::from_secs(15 * 60);
const FORGET_AFTER: Duration = Duration::from_secs(60 * 60);
const MAX_TRACKED: usize = 1024;

impl AuthThrottle {
    pub(crate) const fn new() -> Self {
        AuthThrottle {
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    /// Remaining lockout for `ip`, if any.
    pub(crate) fn locked_for(&self, ip: IpAddr, now: Instant) -> Option<Duration> {
        let e = lock(&self.entries);
        let until = e.get(&ip.to_canonical())?.locked_until?;
        until.checked_duration_since(now).filter(|d| !d.is_zero())
    }

    /// Records a failed attempt and returns the failure count.
    pub(crate) fn record_failure(
        &self,
        ip: IpAddr,
        now: Instant,
        max_failures: u32,
        base_lockout: Duration,
    ) -> u32 {
        let ip = ip.to_canonical();
        let mut e = lock(&self.entries);
        if e.len() >= MAX_TRACKED && !e.contains_key(&ip) {
            e.retain(|_, v| {
                v.locked_until.is_some_and(|u| u > now)
                    || now.saturating_duration_since(v.last_failure) < FORGET_AFTER
            });
            if e.len() >= MAX_TRACKED {
                // still full of live entries: forget the stalest one
                if let Some(oldest) = e
                    .iter()
                    .min_by_key(|(_, v)| v.last_failure)
                    .map(|(k, _)| *k)
                {
                    e.remove(&oldest);
                }
            }
        }
        let ent = e.entry(ip).or_insert(AuthEntry {
            failures: 0,
            locked_until: None,
            last_failure: now,
        });
        if now.saturating_duration_since(ent.last_failure) >= FORGET_AFTER {
            ent.failures = 0;
        }
        ent.failures = ent.failures.saturating_add(1);
        ent.last_failure = now;
        if max_failures > 0 && ent.failures >= max_failures {
            let doublings = (ent.failures - max_failures).min(16);
            let len = base_lockout
                .checked_mul(1u32 << doublings)
                .unwrap_or(MAX_LOCKOUT)
                .min(MAX_LOCKOUT);
            ent.locked_until = Some(now + len);
        }
        ent.failures
    }

    pub(crate) fn record_success(&self, ip: IpAddr) {
        lock(&self.entries).remove(&ip.to_canonical());
    }
}

pub(crate) fn rcon_max_failures() -> u32 {
    env_parse("MEGU_RCON_MAX_FAILURES").unwrap_or(5)
}

pub(crate) fn rcon_lockout() -> Duration {
    Duration::from_secs(env_parse("MEGU_RCON_LOCKOUT_SECS").unwrap_or(30).max(1))
}

// ------------------------------------------------------------- tick supervision

/// Runs `f`, turning a panic into an `Err` with the panic message. The tick
/// thread uses this so a panicking tick is logged and ends the process instead
/// of leaving the listener alive with a dead tick.
///
/// Note on poisoned locks: after a panic, every `Mutex` the panicking thread
/// held is poisoned, and the many `lock().unwrap()` calls elsewhere would
/// panic in turn. That is deliberately not "fixed" here; the process exits
/// instead of limping on with half-updated shared state.
pub(crate) fn catch_panic<F: FnOnce()>(f: F) -> Result<(), String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).map_err(|p| {
        p.downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn compression_frames_round_trip_and_reject_forged_lengths() {
        for (raw, threshold) in [
            (b"\x01hello".as_slice(), 10),
            (b"\x01hello".as_slice(), 6),
            (b"\x01hello".as_slice(), 0),
        ] {
            let frame = encode_compressed_frame(raw, threshold);
            let mut body = frame.as_slice();
            let length = crate::read_protocol_i32(&mut body).unwrap() as usize;
            assert_eq!(length, body.len());
            assert_eq!(decode_compressed_frame(body, threshold, 100).unwrap(), raw);
        }
        assert_eq!(set_compression(256), [0x80, 0x02]);
        let frame = encode_compressed_frame(b"\x01hello", 6);
        let mut body = frame.as_slice();
        crate::read_protocol_i32(&mut body).unwrap();
        let mut forged = body.to_vec();
        forged[0] = 7;
        assert!(decode_compressed_frame(&forged, 6, 100).is_err());
        forged[0] = 5;
        assert!(decode_compressed_frame(&forged, 6, 100).is_err());
        assert!(decode_compressed_frame(body, 6, 5).is_err());
        assert!(decode_compressed_frame(&[0, 1, 2], 2, 100).is_err());
        assert!(decode_compressed_frame(&[0, 1, 2], 3, 1).is_err());
        assert!(decode_compressed_frame(&[0x80], 1, 100).is_err());
        assert!(decode_compressed_frame(&body[..body.len() - 1], 6, 100).is_err());
        let mut trailing = body.to_vec();
        trailing.push(0);
        assert!(decode_compressed_frame(&trailing, 6, 100).is_err());
    }

    // ------------------------------------------------- zlib scratch reusage

    /// Deterministic bytes that resist deflate, so the "incompressible" case is
    /// reproducible instead of a real RNG draw.
    fn pseudo_random(len: usize, seed: u32) -> Vec<u8> {
        let mut state = seed | 1;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect()
    }

    /// zlib's Adler-32, the checksum a stream's trailer carries.
    fn adler32(data: &[u8]) -> u32 {
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in data {
            a = (a + u32::from(byte)) % 65_521;
            b = (b + a) % 65_521;
        }
        (b << 16) | a
    }

    /// A stream is only accepted by a conforming client when the header names
    /// deflate with a 32 KiB window, its FCHECK bits make the header divisible
    /// by 31, and the trailer is Adler-32 over the plain input. Check all three,
    /// so "valid zlib" is measured rather than assumed from a round trip.
    fn assert_valid_zlib_stream(stream: &[u8], plain: &[u8]) {
        assert!(
            stream.len() >= 6,
            "a zlib stream has a header and a trailer"
        );
        assert_eq!(stream[0], 0x78, "deflate, 32 KiB window (CMF)");
        let header = (u16::from(stream[0]) << 8) | u16::from(stream[1]);
        assert_eq!(header % 31, 0, "FCHECK makes the header divisible by 31");
        let trailer = u32::from_be_bytes(
            stream[stream.len() - 4..]
                .try_into()
                .expect("four trailer bytes"),
        );
        assert_eq!(trailer, adler32(plain), "Adler-32 trailer covers the input");
    }

    #[test]
    fn zlib_scratch_matches_a_fresh_encoder_byte_for_byte() {
        use std::io::Write as _;

        // Wire compatibility: a reused `Compress` after `reset()` must emit
        // exactly what the per-packet `ZlibEncoder` this replaced emitted, at
        // every level `MEGU_ZLIB_LEVEL` can select.
        let mut chunk = Vec::new();
        for i in 0..10_240u32 {
            chunk.extend_from_slice(&[b'a' + (i % 26) as u8, (i >> 8) as u8]);
        }
        for level in 1..=9u32 {
            for raw in [&[][..], b"\x01hello".as_slice(), chunk.as_slice()] {
                let mut fresh = Vec::new();
                let mut encoder =
                    flate2::write::ZlibEncoder::new(&mut fresh, flate2::Compression::new(level));
                encoder
                    .write_all(raw)
                    .expect("writing to a Vec cannot fail");
                encoder.finish().expect("writing to a Vec cannot fail");
                let reused = with_zlib_scratch(|scratch| scratch.compress_at(level, raw).to_vec());
                assert_eq!(reused, fresh, "level {level}, {} plain bytes", raw.len());
                assert_valid_zlib_stream(&reused, raw);
            }
        }
    }

    #[test]
    fn zlib_scratch_round_trips_every_payload_shape_and_threshold() {
        let repetitive = vec![0x5a_u8; 40 * 1024];
        let random = pseudo_random(40 * 1024, 0x1234_5678);
        let tiny = pseudo_random(37, 7);
        let payloads: [&[u8]; 5] = [&[], b"\x01hello", &tiny, &repetitive, &random];
        for raw in payloads {
            for threshold in [0usize, 1, 37, 256, 4096, 1 << 20] {
                // An empty payload under a 0 threshold is the one shape the
                // wire form cannot express: `varint(0)` is the uncompressed
                // marker, so an empty compressed body would be read as plain.
                // Pre-existing and unreachable (a packet always has an id).
                if raw.is_empty() && threshold == 0 {
                    continue;
                }
                let frame = encode_compressed_frame(raw, threshold);
                let mut body = frame.as_slice();
                let length = crate::read_protocol_i32(&mut body).expect("outer length") as usize;
                assert_eq!(length, body.len(), "outer frame length is exact");
                let mut inner = body;
                let data_len = crate::read_protocol_i32(&mut inner).expect("data length");
                let decoded =
                    decode_compressed_frame(body, threshold, 1 << 20).unwrap_or_else(|e| {
                        panic!("{} bytes at threshold {threshold}: {e}", raw.len())
                    });
                assert_eq!(decoded, raw, "{} bytes at threshold {threshold}", raw.len());
                if raw.len() >= threshold {
                    assert_eq!(data_len as usize, raw.len(), "compressed path taken");
                    assert_valid_zlib_stream(inner, raw);
                } else {
                    assert_eq!(data_len, 0, "below threshold sends a 0 data length");
                    assert_eq!(inner, raw, "below threshold sends the plain bytes");
                }
            }
        }
    }

    #[test]
    fn zlib_scratch_reuses_one_buffer_without_stale_bytes() {
        let first = vec![b'A'; 40 * 1024];
        let second: Vec<u8> = (0..32 * 1024).map(|i| (i % 251) as u8).collect();

        let (cap0, ptr0, bytes0) = with_zlib_scratch(|scratch| {
            let bytes = scratch.compress_at(1, &first).to_vec();
            (scratch.out.capacity(), scratch.out.as_ptr(), bytes)
        });
        // A differently sized packet in between must not leak a tail into the
        // next stream and must not reallocate the buffer.
        let (cap1, ptr1, bytes1) = with_zlib_scratch(|scratch| {
            let bytes = scratch.compress_at(1, &second).to_vec();
            (scratch.out.capacity(), scratch.out.as_ptr(), bytes)
        });
        let (cap2, ptr2, bytes2) = with_zlib_scratch(|scratch| {
            let bytes = scratch.compress_at(1, &first).to_vec();
            (scratch.out.capacity(), scratch.out.as_ptr(), bytes)
        });
        assert_eq!(
            bytes0, bytes2,
            "the same input compresses to the same bytes"
        );
        assert_valid_zlib_stream(&bytes0, &first);
        assert_valid_zlib_stream(&bytes1, &second);
        assert_eq!((cap1, ptr1), (cap0, ptr0), "the output buffer was reused");
        assert_eq!(
            (cap2, ptr2),
            (cap0, ptr0),
            "a smaller packet does not reallocate it either"
        );
        assert!(
            cap0 <= ZLIB_SCRATCH_RETAIN_CAP,
            "steady state stays under the retention cap: {cap0}"
        );

        // The reused stream still decodes to exactly its own packet.
        let mut body = Vec::new();
        crate::write_varint(&mut body, second.len() as i64);
        body.extend_from_slice(&bytes1);
        assert_eq!(decode_compressed_frame(&body, 1, 1 << 20).unwrap(), second);
    }

    #[test]
    fn zlib_scratch_releases_an_oversized_spike_buffer() {
        let spike = pseudo_random(2 * 1024 * 1024, 0xabcd_ef01);
        let spike_cap = with_zlib_scratch(|scratch| {
            let bytes = scratch.compress_at(1, &spike);
            assert_valid_zlib_stream(bytes, &spike);
            scratch.out.capacity()
        });
        assert!(
            spike_cap > ZLIB_SCRATCH_RETAIN_CAP,
            "an incompressible 2 MiB packet needs more than the cap: {spike_cap}"
        );
        let (cap, len, ptr) = with_zlib_scratch(|scratch| {
            let bytes = scratch.compress_at(1, b"\x01hello");
            assert_valid_zlib_stream(bytes, b"\x01hello");
            (
                scratch.out.capacity(),
                scratch.out.len(),
                scratch.out.as_ptr(),
            )
        });
        assert!(
            cap <= ZLIB_SCRATCH_RETAIN_CAP,
            "the spike's buffer was released: {cap}"
        );
        assert!(len < 64, "only the next packet lives in it: {len}");
        let ptr_again = with_zlib_scratch(|scratch| {
            scratch.compress_at(1, b"\x01hello");
            scratch.out.as_ptr()
        });
        assert_eq!(ptr_again, ptr, "the replacement buffer is reused from here");
    }

    #[test]
    fn zlib_scratch_inflate_reuses_state_and_keeps_fail_closed_guards() {
        let plain = vec![b'q'; 12 * 1024];
        let mut body = Vec::new();
        crate::write_varint(&mut body, plain.len() as i64);
        with_zlib_scratch(|scratch| {
            body.extend_from_slice(scratch.compress_at(1, &plain));
        });

        // Decoding repeatedly proves the cached inflate was reset: a stale
        // `total_in` would trip the trailing-byte guard on the second call.
        for _ in 0..4 {
            assert_eq!(decode_compressed_frame(&body, 256, 1 << 20).unwrap(), plain);
        }

        // A stream longer than the data length claims — a bomb in miniature.
        let mut bomb = Vec::new();
        crate::write_varint(&mut bomb, 4096);
        with_zlib_scratch(|scratch| bomb.extend_from_slice(scratch.compress_at(1, &plain)));
        assert!(decode_compressed_frame(&bomb, 1, 1 << 20).is_err());

        // Trailing bytes after a complete stream.
        let mut trailing = body.clone();
        trailing.push(0);
        assert!(decode_compressed_frame(&trailing, 256, 1 << 20).is_err());

        // A truncated stream leaves nothing behind for the next packet.
        assert!(decode_compressed_frame(&body[..body.len() - 1], 256, 1 << 20).is_err());
        assert_eq!(decode_compressed_frame(&body, 256, 1 << 20).unwrap(), plain);
    }

    #[test]
    fn zlib_scratch_is_shared_by_chunk_cache_and_broadcast_paths() {
        let payload: Vec<u8> = (0..40 * 1024).map(|i| (i % 251) as u8).collect();

        // A chunk-cache miss compresses first and sizes the retained buffer.
        let mut cache = ChunkPacketCache::new();
        let key = (crate::DimId::Overworld, 4, -7, false);
        cache.insert(1, key, 9, Vec::new(), payload.clone());
        let (cached, _) = cache.frame(1, key, 9, Some(256)).unwrap();
        let (cap, ptr) = with_zlib_scratch(|s| (s.out.capacity(), s.out.as_ptr()));

        // Every other in-file encoder takes the same thread-local state.
        let uncached = ChunkPacketCache::frame_uncached(&payload, Some(256));
        assert_eq!(
            with_zlib_scratch(|s| (s.out.capacity(), s.out.as_ptr())),
            (cap, ptr),
            "an uncached send reused the chunk-cache buffer"
        );
        let mut shared = SharedBroadcastFrame::new(0x27, &payload);
        let broadcast = shared.frame_for(Some(256));
        assert_eq!(
            with_zlib_scratch(|s| (s.out.capacity(), s.out.as_ptr())),
            (cap, ptr),
            "the broadcast fanout reused the same buffer"
        );

        assert_eq!(&*broadcast, &*uncached, "identical bytes from both paths");
        assert_eq!(&*cached, &*broadcast, "the cache stores the same frame");
        let mut body = cached.as_slice();
        crate::read_protocol_i32(&mut body).expect("outer length");
        let decoded = decode_compressed_frame(body, 256, 1 << 20).unwrap();
        assert_eq!(&decoded[1..], &payload[..], "the frame holds the payload");
    }

    #[test]
    fn zlib_scratch_is_per_thread_and_never_crosses_payloads() {
        let handles: Vec<_> = (0..8u8)
            .map(|lane| {
                std::thread::spawn(move || {
                    let payload = vec![lane; 1024 * (usize::from(lane) + 1)];
                    for _ in 0..20 {
                        let frame = encode_compressed_frame(&payload, 256);
                        let mut body = frame.as_slice();
                        crate::read_protocol_i32(&mut body).expect("outer length");
                        assert_eq!(
                            decode_compressed_frame(body, 256, 1 << 20).expect("valid frame"),
                            payload,
                            "lane {lane} round-trips its own payload"
                        );
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("worker thread finished");
        }
    }

    #[test]
    fn relative_movement_limits_and_payloads() {
        let delta = relative_move_delta([0.0; 3], [-8.0, 1.5, 32767.0 / 4096.0]).unwrap();
        assert_eq!(delta, [i16::MIN, 6144, i16::MAX]);
        assert_eq!(relative_move_delta([0.0; 3], [8.0, 0.0, 0.0]), None);
        assert_eq!(relative_move_delta([0.0; 3], [-8.001, 0.0, 0.0]), None);
        assert_eq!(relative_move_delta([0.0; 3], [f64::NAN, 0.0, 0.0]), None);
        // Crossing a quantization boundary can advance by one step even for
        // a change under half a step. Java rounds negative ties toward +∞.
        assert_eq!(
            relative_move_delta(
                [0.49 / 4096.0, 0.0, 0.0],
                [0.51 / 4096.0, -0.5 / 4096.0, 0.0]
            ),
            Some([1, 0, 0])
        );
        assert_eq!(
            entity_relative_move(300, delta, None, true),
            [0xac, 0x02, 0x80, 0, 0x18, 0, 0x7f, 0xff, 1]
        );
        assert_eq!(
            entity_relative_move(300, delta, Some((0xc0, 0x40)), false),
            [0xac, 0x02, 0x80, 0, 0x18, 0, 0x7f, 0xff, 0xc0, 0x40, 0]
        );
        assert_eq!(
            entity_rotation(300, 0xc0, 0x40, true),
            [0xac, 0x02, 0xc0, 0x40, 1]
        );
        assert_eq!(entity_head_rotation(300, 0xc0), [0xac, 0x02, 0xc0]);
    }

    fn ip(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn idle_timeout_can_be_changed_and_disabled() {
        let previous = idle_timeout();
        set_idle_timeout(Duration::from_secs(120));
        assert_eq!(idle_timeout(), Duration::from_secs(120));
        set_idle_timeout(Duration::ZERO);
        assert!(idle_timeout().is_zero());
        set_idle_timeout(previous);
    }

    #[test]
    fn loopback_binds_are_allowed_everything_else_needs_opt_in() {
        for ok in [
            "127.0.0.1",
            "127.0.0.2",
            "::1",
            "[::1]",
            "localhost",
            "LOCALHOST",
        ] {
            assert!(check_bind("x", ok, false).is_ok(), "{ok}");
        }
        for bad in [
            "0.0.0.0",
            "::",
            "192.168.1.5",
            "10.0.0.1",
            "example.org",
            "",
        ] {
            let e = check_bind("game listener", bad, false).unwrap_err();
            assert!(e.contains("MEGU_ALLOW_PUBLIC_BIND"), "{bad}: {e}");
            assert!(check_bind("x", bad, true).is_ok(), "{bad} with opt-in");
        }
        assert!(truthy("1") && truthy("TRUE") && !truthy("0") && !truthy(""));
    }

    #[test]
    fn per_ip_limit_defaults_skip_loopback_but_explicit_applies_everywhere() {
        assert_eq!(per_ip_limit_with(ip(127, 0, 0, 1), None), 0);
        assert_eq!(per_ip_limit_with(ip(203, 0, 113, 9), None), 4);
        assert_eq!(per_ip_limit_with(ip(127, 0, 0, 1), Some(2)), 2);
        assert_eq!(per_ip_limit_with(ip(203, 0, 113, 9), Some(0)), 0);
    }

    #[test]
    fn per_ip_slots_are_capped_and_released() {
        static T: IpConnections = IpConnections::new();
        let a = ip(203, 0, 113, 1);
        let b = ip(203, 0, 113, 2);
        let s1 = T.acquire(a, 2).unwrap();
        let _s2 = T.acquire(a, 2).unwrap();
        assert!(T.acquire(a, 2).is_none(), "third from the same IP");
        assert!(T.acquire(b, 2).is_some(), "other IPs are independent");
        drop(s1);
        assert!(T.acquire(a, 2).is_some(), "a freed slot is reusable");
        // limit 0 = unlimited
        let held: Vec<_> = (0..20)
            .map(|_| T.acquire(ip(10, 1, 1, 1), 0).unwrap())
            .collect();
        assert_eq!(T.count(ip(10, 1, 1, 1)), 20);
        drop(held);
        assert_eq!(T.count(ip(10, 1, 1, 1)), 0);
    }

    #[test]
    fn a_panicking_handler_still_releases_its_ip_slot() {
        static T: IpConnections = IpConnections::new();
        let a = ip(198, 51, 100, 7);
        let r = std::panic::catch_unwind(|| {
            let _slot = T.acquire(a, 1).unwrap();
            panic!("synthetic");
        });
        assert!(r.is_err());
        assert_eq!(T.count(a), 0);
    }

    #[test]
    fn mapped_ipv6_counts_as_the_same_ipv4_peer() {
        static T: IpConnections = IpConnections::new();
        let v4 = ip(203, 0, 113, 50);
        let mapped: IpAddr = "::ffff:203.0.113.50".parse().unwrap();
        let _s = T.acquire(v4, 1).unwrap();
        assert!(T.acquire(mapped, 1).is_none());
    }

    #[test]
    fn duplicate_name_or_uuid_is_refused_until_the_first_session_ends() {
        static R: LoginRegistry = LoginRegistry::new();
        let first = R.reserve("Steve", [1; 16]).unwrap();
        assert_eq!(
            R.reserve("Steve", [2; 16]).err(),
            Some(LoginRefusal::NameInUse)
        );
        assert_eq!(
            R.reserve("sTEVE", [2; 16]).err(),
            Some(LoginRefusal::NameInUse)
        );
        assert_eq!(
            R.reserve("Alex", [1; 16]).err(),
            Some(LoginRefusal::UuidInUse)
        );
        let other = R.reserve("Alex", [2; 16]).unwrap();
        drop(first);
        let again = R.reserve("Steve", [1; 16]).unwrap();
        drop(other);
        drop(again);
        assert!(R.reserve("Alex", [2; 16]).is_ok());
    }

    #[test]
    fn allowlist_is_off_when_empty_and_matches_names_or_uuids() {
        assert!(Allowlist::parse("").is_none());
        assert!(Allowlist::parse(" , ,").is_none());
        let l = Allowlist::parse("Steve, alex 01234567-89ab-cdef-0123-456789abcdef").unwrap();
        assert!(l.permits("steve", &[0; 16]));
        assert!(l.permits("ALEX", &[0; 16]));
        assert!(!l.permits("Mallory", &[0; 16]));
        let u = parse_uuid("0123456789abcdef0123456789abcdef").unwrap();
        assert!(l.permits("Mallory", &u), "UUID entry permits any name");
        assert!(parse_uuid("not-a-uuid").is_none());
    }

    #[test]
    fn chat_text_follows_vanilla_validity() {
        assert!(chat_text_valid("hello world"));
        assert!(chat_text_valid(&"a".repeat(256)));
        assert!(!chat_text_valid(&"a".repeat(257)));
        assert!(
            chat_text_valid(&"\u{3042}".repeat(256)),
            "256 chars, not bytes"
        );
        assert!(!chat_text_valid("bad \u{a7}c colour"));
        assert!(!chat_text_valid("tab\there"));
        assert!(!chat_text_valid("nul\0"));
        assert!(!chat_text_valid("del\u{7f}"));
    }

    #[test]
    fn chat_burst_of_ten_passes_eleventh_is_spam_and_it_drains() {
        let t0 = Instant::now();
        let mut t = ChatThrottle::new(t0);
        for i in 0..10 {
            assert!(t.allow(t0), "message {i}");
        }
        assert!(!t.allow(t0), "11th in the same instant");
        // a patient player is never kicked: one message per second forever
        let mut t = ChatThrottle::new(t0);
        for i in 0..500u64 {
            assert!(t.allow(t0 + Duration::from_secs(i)), "message {i}");
        }
        // after a kick-worthy burst, draining 20 points (1 s) buys one message
        let mut t = ChatThrottle::new(t0);
        for _ in 0..10 {
            t.allow(t0);
        }
        assert!(t.allow(t0 + Duration::from_secs(1)));
        assert!(!t.allow(t0 + Duration::from_secs(1)));
    }

    #[test]
    fn packet_rate_limit_admits_teleport_ack_and_creative_bursts() {
        let now = Instant::now();
        let mut limit = PacketRateLimit::new(now);
        for i in 0..1200 {
            assert!(limit.allow(now), "burst packet {i}");
        }
        assert!(!limit.allow(now));
        assert_eq!(limit.dropped(), 1);
        assert!(!limit.should_disconnect(now));
    }

    #[test]
    fn packet_rate_limit_refills_precisely_with_injected_time() {
        let now = Instant::now();
        let mut limit = PacketRateLimit::new(now);
        for _ in 0..1200 {
            assert!(limit.allow(now));
        }
        let quarter = now + Duration::from_millis(250);
        for _ in 0..125 {
            assert!(limit.allow(quarter));
        }
        assert!(!limit.allow(quarter));
        let restored = now + Duration::from_secs(3);
        for _ in 0..1200 {
            assert!(limit.allow(restored));
        }
        assert!(!limit.allow(restored));
    }

    #[test]
    fn packet_rate_limit_disconnects_sustained_flood_but_recovers_after_quiet() {
        let now = Instant::now();
        let mut limit = PacketRateLimit::new(now);
        for _ in 0..1200 {
            assert!(limit.allow(now));
        }
        for ms in (0..=2100).step_by(10) {
            let at = now + Duration::from_millis(ms);
            // 100 packets per 10ms overwhelms the 500/s replenishment;
            // occasional admitted packets must not reset the overage window.
            for _ in 0..100 {
                limit.allow(at);
            }
            if ms < 2000 {
                assert!(!limit.should_disconnect(at));
            }
        }
        assert!(limit.should_disconnect(now + Duration::from_millis(2100)));
        assert!(limit.dropped() > 10_000);

        let later = now + Duration::from_secs(5);
        assert!(limit.allow(later));
        assert!(!limit.should_disconnect(later));
    }

    #[test]
    fn constant_time_compare_is_correct() {
        assert!(ct_eq(b"hunter2", b"hunter2"));
        assert!(!ct_eq(b"hunter2", b"hunter3"));
        assert!(!ct_eq(b"hunter2", b"hunter"));
        assert!(!ct_eq(b"hunter", b"hunter2"));
        assert!(!ct_eq(b"", b"x"));
        assert!(ct_eq(b"", b""));
        assert!(
            !ct_eq(&[b'a'; 256], &[b'a'; 512]),
            "length difference beyond a byte"
        );
        assert!(!ct_eq(&[b'a'; 256], b""));
    }

    #[test]
    fn rcon_failures_lock_the_ip_out_with_growing_delay_and_success_clears() {
        let th = AuthThrottle::new();
        let a = ip(203, 0, 113, 9);
        let b = ip(203, 0, 113, 10);
        let t0 = Instant::now();
        let base = Duration::from_secs(30);
        for i in 1..5 {
            assert_eq!(th.record_failure(a, t0, 5, base), i);
            assert!(
                th.locked_for(a, t0).is_none(),
                "failure {i} is below the threshold"
            );
        }
        assert_eq!(th.record_failure(a, t0, 5, base), 5);
        assert_eq!(th.locked_for(a, t0), Some(base));
        assert!(th.locked_for(b, t0).is_none(), "other IPs unaffected");
        assert!(th
            .locked_for(a, t0 + base + Duration::from_secs(1))
            .is_none());
        // the next failure doubles the lockout
        th.record_failure(a, t0 + base, 5, base);
        assert_eq!(th.locked_for(a, t0 + base), Some(base * 2));
        // capped
        for _ in 0..40 {
            th.record_failure(a, t0 + base, 5, base);
        }
        assert_eq!(th.locked_for(a, t0 + base), Some(MAX_LOCKOUT));
        th.record_success(a);
        assert!(th.locked_for(a, t0 + base).is_none());
        assert_eq!(th.record_failure(a, t0 + base, 5, base), 1);
    }

    #[test]
    fn rcon_failure_table_stays_bounded() {
        let th = AuthThrottle::new();
        let t0 = Instant::now();
        for i in 0..(MAX_TRACKED as u32 * 2) {
            let ip = IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + i));
            th.record_failure(
                ip,
                t0 + Duration::from_millis(i as u64),
                5,
                Duration::from_secs(30),
            );
        }
        assert!(lock(&th.entries).len() <= MAX_TRACKED);
    }

    #[test]
    fn catch_panic_reports_the_message() {
        assert_eq!(catch_panic(|| {}), Ok(()));
        assert_eq!(catch_panic(|| panic!("boom")), Err("boom".to_string()));
        let n = 7;
        assert_eq!(catch_panic(move || panic!("n={n}")), Err("n=7".to_string()));
    }

    #[test]
    fn chunk_packet_cache_shares_frames_until_the_revision_moves() {
        let mut cache = ChunkPacketCache::new();
        let key = (crate::DimId::Overworld, 4, -7, false);
        assert!(cache.frame(10, key, 41, None).is_none());
        cache.insert(10, key, 41, vec![((1, 2, 3), 15)], vec![9, 9, 9]);
        let (f1, emitters) = cache.frame(10, key, 41, None).unwrap();
        let (f2, _) = cache.frame(10, key, 41, None).unwrap();
        assert!(
            Arc::ptr_eq(&f1, &f2),
            "readers at the same revision share one frame"
        );
        assert_eq!(emitters, [((1, 2, 3), 15)]);
        // The frame decodes to a 0x27 packet carrying the payload verbatim.
        let mut body = &f1[..];
        crate::read_protocol_i32(&mut body);
        assert_eq!(crate::read_protocol_i32(&mut body), Some(0x27));
        assert_eq!(body, [9, 9, 9]);
        // Compressed readers get a lazily built, then shared, compressed frame.
        let (c1, _) = cache.frame(10, key, 41, Some(256)).unwrap();
        let (c2, _) = cache.frame(10, key, 41, Some(256)).unwrap();
        assert!(Arc::ptr_eq(&c1, &c2));
        assert_ne!(f1, c1);
        let mut framed = &c1[..];
        crate::read_protocol_i32(&mut framed);
        let body = decode_compressed_frame(framed, 256, 1 << 20).unwrap();
        let mut payload = &body[..];
        assert_eq!(crate::read_protocol_i32(&mut payload), Some(0x27));
        assert_eq!(payload, [9, 9, 9]);
        // A different compression mode is a miss: the plain buffer was
        // dropped on the first compressed request, so the caller rebuilds
        // and re-inserts instead of the cache holding two wire forms.
        assert!(cache.frame(10, key, 41, Some(64)).is_none());
        assert!(cache.frame(10, key, 41, None).is_none());
        // The matching threshold still hits the shared compressed frame.
        let (c4, _) = cache.frame(10, key, 41, Some(256)).unwrap();
        assert!(Arc::ptr_eq(&c1, &c4));
        // A block/light revision bump (what set_block* produces) invalidates.
        assert!(cache.frame(10, key, 42, None).is_none());
        cache.insert(10, key, 42, vec![], vec![7]);
        let (f3, _) = cache.frame(10, key, 42, None).unwrap();
        assert_ne!(f1, f3);
        assert!(Arc::ptr_eq(&f3, &cache.frame(10, key, 42, None).unwrap().0));
        // frame_uncached emits identical bytes without touching the entry.
        assert_eq!(*ChunkPacketCache::frame_uncached(&[9, 9, 9], None), *f1);
    }

    #[test]
    fn chunk_packet_cache_drops_everything_on_a_generation_change() {
        let mut cache = ChunkPacketCache::new();
        let key = (crate::DimId::Overworld, 0, 0, false);
        cache.insert(10, key, 1, vec![], vec![1]);
        assert!(cache.frame(11, key, 1, None).is_none());
        cache.insert(11, key, 1, vec![], vec![2]);
        let (frame, _) = cache.frame(11, key, 1, None).unwrap();
        let mut body = &frame[..];
        crate::read_protocol_i32(&mut body);
        crate::read_protocol_i32(&mut body);
        assert_eq!(body, [2]);
    }

    #[test]
    fn chunk_packet_cache_stays_bounded() {
        let mut cache = ChunkPacketCache::new();
        for i in 0..(CHUNK_PACKET_CACHE_CAP + 4) {
            cache.insert(
                1,
                (crate::DimId::Overworld, i as i32, 0, false),
                1,
                vec![],
                vec![i as u8],
            );
        }
        assert!(cache.map.len() <= CHUNK_PACKET_CACHE_CAP);
        assert!(cache
            .frame(1, (crate::DimId::Overworld, 0, 0, false), 1, None)
            .is_none());
    }

    #[test]
    fn broadcast_radius_reads_the_vanilla_range_contract() {
        // named sound: id+1 varint, source varint, fixed-point pos, f32 volume.
        let mut snd = Vec::new();
        crate::write_varint(&mut snd, 763 + 1);
        crate::write_varint(&mut snd, 0);
        for v in [8i32, 520, -16] {
            snd.extend_from_slice(&v.to_be_bytes());
        }
        snd.extend_from_slice(&1.0f32.to_be_bytes());
        assert_eq!(broadcast_radius(0x68, &snd), Some(16.0));
        let mut loud = snd.clone();
        loud.truncate(loud.len() - 4);
        loud.extend_from_slice(&4.0f32.to_be_bytes());
        assert_eq!(broadcast_radius(0x68, &loud), Some(64.0));
        // particles reach 512 only with the long-distance flag, else 32.
        assert_eq!(broadcast_radius(0x29, &[1]), Some(512.0));
        assert_eq!(broadcast_radius(0x29, &[0]), Some(32.0));
        assert_eq!(broadcast_radius(0x20, &[0; 24]), Some(64.0));
        // a packet with no positioned contract is refused, not sent wide.
        assert_eq!(broadcast_radius(0x6c, &[1, 2, 3]), None);
        assert_eq!(broadcast_radius(0x68, &[0]), None);
    }

    #[test]
    fn broadcast_membership_is_a_strict_radius_sphere() {
        assert!(within_broadcast_range(0.0, 0.0, 0.0, 15.99, 0.0, 0.0, 16.0));
        assert!(
            !within_broadcast_range(0.0, 0.0, 0.0, 16.0, 0.0, 0.0, 16.0),
            "vanilla's `dist² < r²` is strict at the boundary"
        );
        assert!(within_broadcast_range(0.0, 0.0, 0.0, 8.0, 8.0, 8.0, 16.0));
        assert!(!within_broadcast_range(0.0, 0.0, 0.0, 9.3, 9.3, 9.3, 16.0));
    }

    #[test]
    fn p7a_parity_mutex_poison_never_cascades_into_connection_limits() {
        // A handler that panics while holding the mutex leaves it poisoned.
        // Every acquisition in this module goes through `lock()`, which recovers
        // the guard, so the next unrelated request is served instead of
        // cascade-panicking.
        static CONNS: IpConnections = IpConnections::new();
        static LOGINS: LoginRegistry = LoginRegistry::new();
        let auth = AuthThrottle::new();
        let peer = ip(203, 0, 113, 77);

        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut c = lock(&CONNS.counts);
            c.insert(peer, 1); // state the panic commits still lands
            let mut h = lock(&LOGINS.held);
            h.push(("steve".to_owned(), [9; 16]));
            let mut e = lock(&auth.entries);
            e.insert(
                peer,
                AuthEntry {
                    failures: 3,
                    locked_until: None,
                    last_failure: Instant::now(),
                },
            );
            panic!("synthetic handler panic while holding three locks");
        }));
        assert!(r.is_err());

        // Per-IP slots still acquire and release; the panic's partial write is
        // visible as it would be after any mid-update disconnect.
        assert_eq!(CONNS.count(peer), 1);
        let slot = CONNS.acquire(peer, 2).unwrap();
        assert_eq!(CONNS.count(peer), 2);
        drop(slot);
        assert_eq!(CONNS.count(peer), 1);
        // The login guard still refuses the name the panic left reserved.
        assert_eq!(
            LOGINS.reserve("Steve", [1; 16]).err(),
            Some(LoginRefusal::NameInUse)
        );
        assert!(LOGINS.reserve("Alex", [1; 16]).is_ok());
        // And the auth table still tracks and locks out.
        let t0 = Instant::now();
        assert_eq!(
            auth.record_failure(peer, t0, 5, Duration::from_secs(30)),
            4
        );
        assert_eq!(
            auth.record_failure(peer, t0, 5, Duration::from_secs(30)),
            5
        );
        assert_eq!(
            auth.locked_for(peer, t0),
            Some(Duration::from_secs(30))
        );
        auth.record_success(peer);
        assert!(auth.locked_for(peer, t0).is_none());
    }

    #[test]
    fn perf_net_ram_zlib_level_defaults_to_fast_and_clamps() {
        assert_eq!(zlib_level_from(None), 1, "default is Compression::fast");
        assert_eq!(zlib_level_from(Some("")), 1);
        assert_eq!(zlib_level_from(Some("abc")), 1);
        assert_eq!(zlib_level_from(Some("0")), 1, "clamped into 1..=9");
        assert_eq!(zlib_level_from(Some("1")), 1);
        assert_eq!(zlib_level_from(Some("6")), 6);
        assert_eq!(zlib_level_from(Some("9")), 9);
        assert_eq!(zlib_level_from(Some("42")), 9, "clamped at the top");
        assert_eq!(zlib_level(), zlib_level_from(
            std::env::var("MEGU_ZLIB_LEVEL").ok().as_deref()
        ));
    }

    #[test]
    fn perf_net_ram_compressed_frame_roundtrips_at_fast_level() {
        // A compressible chunk-sized payload exercises the compressed path.
        let mut raw = Vec::with_capacity(8 * 1024);
        for i in 0..2048u32 {
            raw.extend_from_slice(&[b'a' + (i % 26) as u8, (i >> 8) as u8]);
        }
        let frame = encode_compressed_frame(&raw, 256);
        // Protocol 767 framing: the outer VarInt length covers the rest.
        let mut body = frame.as_slice();
        let length = crate::read_protocol_i32(&mut body).unwrap() as usize;
        assert_eq!(length, body.len(), "outer frame length is exact");
        // The inner data-length VarInt is the uncompressed length.
        let mut inner = body;
        let data_len = crate::read_protocol_i32(&mut inner).unwrap() as usize;
        assert_eq!(data_len, raw.len(), "compressed path taken");
        assert!(
            inner.len() < raw.len(),
            "level-1 zlib still shrinks compressible chunk data"
        );
        assert_eq!(
            decode_compressed_frame(body, 256, 1 << 20).unwrap(),
            raw,
            "fast level round-trips byte-for-byte"
        );
        // Two allocations total (body + one prepend): the returned Vec has no
        // oversized spare capacity beyond the 5-byte length prefix.
        assert!(frame.capacity() >= frame.len());
        assert!(
            frame.capacity() - frame.len() <= 5,
            "capacity {} vs len {}",
            frame.capacity(),
            frame.len()
        );
        // The uncompressed path keeps identical framing for small packets.
        let small = encode_compressed_frame(b"\x01hello", 10);
        let mut sbody = small.as_slice();
        let slen = crate::read_protocol_i32(&mut sbody).unwrap() as usize;
        assert_eq!(slen, sbody.len());
        assert_eq!(sbody[0], 0, "below threshold sends the 0 data length");
        assert_eq!(&sbody[1..], b"\x01hello");
    }

    #[test]
    fn perf_net_ram_chunk_cache_drops_plain_on_compression() {
        let mut cache = ChunkPacketCache::new();
        let key = (crate::DimId::Overworld, 9, 9, false);
        // A compressible ~20 KiB chunk payload, like a real 0x27 frame.
        let mut payload = Vec::with_capacity(20 * 1024);
        for i in 0..5120u32 {
            payload.extend_from_slice(&[b'x' + (i % 7) as u8, b'y', b'z', b'w']);
        }
        cache.insert(3, key, 5, vec![], payload);
        let before = cache.estimated_bytes();
        assert!(before > 15 * 1024, "plain frame retained: {before}");

        let (c1, _) = cache.frame(3, key, 5, Some(256)).unwrap();
        let after = cache.estimated_bytes();
        eprintln!("perf_net_ram cache RAM: {before} -> {after} bytes");
        assert!(
            after * 2 < before,
            "compressed entry must retain <50% of the plain+compressed pair: {before} -> {after}"
        );
        // Same-threshold hits share the one compressed frame.
        let (c2, _) = cache.frame(3, key, 5, Some(256)).unwrap();
        assert!(Arc::ptr_eq(&c1, &c2));
        // Any other mode misses; the caller rebuilds and re-inserts.
        assert!(cache.frame(3, key, 5, Some(64)).is_none());
        assert!(cache.frame(3, key, 5, None).is_none());
        // The shared frame still decodes to the exact wire body.
        let mut framed = &c1[..];
        crate::read_protocol_i32(&mut framed);
        let body = decode_compressed_frame(framed, 256, 1 << 20).unwrap();
        let mut inner = &body[..];
        assert_eq!(crate::read_protocol_i32(&mut inner), Some(0x27));
        assert_eq!(inner.len(), 20 * 1024);
    }

    #[test]
    fn perf_net_ram_coalescer_batches_frames_into_one_write() {
        use std::io::Write;

        struct Counting {
            bytes: Vec<u8>,
            writes: usize,
        }
        impl Write for Counting {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.writes += 1;
                self.bytes.extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut w = Counting {
            bytes: Vec::new(),
            writes: 0,
        };
        let mut coalescer = PacketCoalescer::new();
        let mut expected = Vec::new();
        for i in 0..20usize {
            let frame = vec![i as u8; 64 + i];
            coalescer.push_frame(&frame, &mut w).unwrap();
            expected.extend_from_slice(&frame);
        }
        assert_eq!(w.writes, 0, "nothing leaves before flush");
        assert_eq!(coalescer.buffered(), expected.len());
        coalescer.flush(&mut w).unwrap();
        assert_eq!(w.writes, 1, "20 frames coalesce into one write");
        assert_eq!(w.bytes, expected, "wire order is byte-for-byte");
        assert_eq!(coalescer.buffered(), 0);
        coalescer.flush(&mut w).unwrap();
        assert_eq!(w.writes, 1, "empty flush is a no-op");

        // The auto-flush threshold bounds the buffer and a frame larger than
        // the whole buffer writes through directly; order still holds.
        let mut w2 = Counting {
            bytes: Vec::new(),
            writes: 0,
        };
        let mut bounded = PacketCoalescer::with_capacity(300);
        let mut expected2 = Vec::new();
        for i in 0..10u8 {
            let frame = vec![0xa0 + i; 100];
            bounded.push_frame(&frame, &mut w2).unwrap();
            expected2.extend_from_slice(&frame);
        }
        let big = vec![0xff; 400];
        bounded.push_frame(&big, &mut w2).unwrap();
        expected2.extend_from_slice(&big);
        bounded.flush(&mut w2).unwrap();
        assert!(w2.writes >= 3, "threshold flushes plus passthrough: {}", w2.writes);
        assert_eq!(w2.bytes, expected2, "auto-flush preserves wire order");
        assert_eq!(bounded.buffered(), 0);
    }

    #[test]
    fn bot_net1_broadcast_frame_encodes_once_for_a_50_player_fanout() {
        // A compressible ~4 KiB payload at the vanilla default threshold.
        let mut payload = Vec::with_capacity(4 * 1024);
        for i in 0..2048u32 {
            payload.extend_from_slice(&[b'a' + (i % 26) as u8, (i >> 8) as u8]);
        }
        let mut body = Vec::new();
        crate::write_varint(&mut body, 0x27);
        body.extend_from_slice(&payload);
        let expected = encode_compressed_frame(&body, 256);

        // 50 clients at the negotiated threshold: one compression, then 50
        // clones of the same allocation.
        let mut shared = SharedBroadcastFrame::new(0x27, &payload);
        let frames: Vec<Arc<[u8]>> = (0..50).map(|_| shared.frame_for(Some(256))).collect();
        assert_eq!(
            shared.encode_count(),
            1,
            "the 50-player broadcast compressed exactly once"
        );
        for frame in &frames {
            assert!(
                Arc::ptr_eq(frame, &frames[0]),
                "every client receives the same shared frame"
            );
            assert_eq!(
                &**frame,
                expected.as_slice(),
                "identical to encode_compressed_frame byte-for-byte"
            );
        }

        // An uncompressed client builds the plain form lazily — a second
        // encode, shared across subsequent plain requests.
        let p1 = shared.frame_for(None);
        let p2 = shared.frame_for(None);
        assert!(Arc::ptr_eq(&p1, &p2));
        assert_eq!(shared.encode_count(), 2);
        let mut expected_plain = Vec::new();
        crate::write_varint(&mut expected_plain, body.len() as i64);
        expected_plain.extend_from_slice(&body);
        assert_eq!(&*p1, expected_plain.as_slice());
        // The compressed fanout is untouched by serving the plain form.
        let c = shared.frame_for(Some(256));
        assert!(Arc::ptr_eq(&c, &frames[0]));
        assert_eq!(shared.encode_count(), 2);

        // A different threshold re-encodes once, then shares that build.
        let other = shared.frame_for(Some(64));
        assert_eq!(shared.encode_count(), 3);
        assert!(!Arc::ptr_eq(&other, &frames[0]));
        assert_eq!(
            &*other,
            encode_compressed_frame(&body, 64).as_slice(),
            "rebuilt at the new threshold"
        );
        let again = shared.frame_for(Some(64));
        assert!(Arc::ptr_eq(&other, &again));
        assert_eq!(shared.encode_count(), 3);
    }

    #[test]
    fn bot_net1_tx_queue_writer_coalesces_a_burst_into_one_flush() {
        use std::io::Write;

        struct Counting {
            writes: usize,
            bytes: Vec<u8>,
        }
        impl Write for Counting {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.writes += 1;
                self.bytes.extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let queue = Arc::new(ClientTxQueue::default());
        let mut expected = Vec::new();
        // Queue the whole burst before the writer starts so it wakes to one
        // 30-frame batch — the deterministic form of the drain+flush-once
        // path a mid-tick burst hits.
        for i in 0..30u8 {
            let frame: Arc<[u8]> = Arc::from(vec![i; 100 + usize::from(i)]);
            expected.extend_from_slice(&frame);
            queue.try_enqueue(frame).unwrap();
        }
        assert_eq!(queue.queued_bytes(), expected.len());

        let sink = Arc::new(Mutex::new(Counting {
            writes: 0,
            bytes: Vec::new(),
        }));
        let writer = {
            let queue = queue.clone();
            let sink = sink.clone();
            std::thread::spawn(move || {
                let mut out = lock(&sink);
                queue.writer_loop(&mut *out)
            })
        };
        while queue.queued_bytes() != 0 && !writer.is_finished() {
            std::thread::yield_now();
        }
        // close() wakes the writer to exit cleanly; the join proves the
        // thread is gone — zero leaked threads.
        queue.close();
        writer.join().unwrap().unwrap();

        let out = lock(&sink);
        assert_eq!(out.writes, 1, "the 30-frame burst is one coalesced write");
        assert_eq!(out.bytes, expected, "frames leave in exact wire order");
        assert_eq!(
            queue.try_enqueue(Arc::from(vec![1u8])),
            Err(TxQueueError::Closed)
        );
    }

    #[test]
    fn bot_net1_tx_queue_close_wakes_the_writer_on_an_empty_queue() {
        let queue = Arc::new(ClientTxQueue::new(1024));
        let writer = {
            let queue = queue.clone();
            std::thread::spawn(move || {
                let mut sink = Vec::new();
                queue.writer_loop(&mut sink).map(|_| sink)
            })
        };
        queue.close();
        let sink = writer.join().unwrap().unwrap();
        assert!(sink.is_empty(), "nothing queued means nothing written");
    }

    #[test]
    fn bot_net1_tx_queue_backpressure_fails_fast_without_blocking() {
        // A stalled client: no writer drains, so the byte budget fills and
        // the producer — game_tick — must see the refusal immediately rather
        // than wait on the socket.
        let queue = ClientTxQueue::new(300);
        let frame: Arc<[u8]> = Arc::from(vec![7u8; 100]);
        for _ in 0..3 {
            queue.try_enqueue(frame.clone()).unwrap();
        }
        assert_eq!(queue.queued_bytes(), 300);

        let start = Instant::now();
        assert_eq!(
            queue.try_enqueue(Arc::from(vec![9u8; 1])),
            Err(TxQueueError::BackpressureSlowClient)
        );
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "the slow client never blocks the producer"
        );
        // The refusal is accounted, not queued: the budget is unchanged and
        // even an old frame still over the limit keeps failing.
        assert_eq!(queue.queued_bytes(), 300);
        assert_eq!(
            queue.try_enqueue(frame.clone()),
            Err(TxQueueError::BackpressureSlowClient)
        );

        // Once a drain frees budget the producer queues again, in FIFO order.
        let mut out = Vec::new();
        queue.drain_batch(&mut out);
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|f| Arc::ptr_eq(f, &frame)));
        assert_eq!(queue.queued_bytes(), 0);
        queue.try_enqueue(frame).unwrap();

        queue.close();
        assert_eq!(
            queue.try_enqueue(Arc::from(vec![1u8; 1])),
            Err(TxQueueError::Closed)
        );
    }
}
