//! Connection-level limits and abuse guards for the listeners.
//!
//! Everything here is pure policy plus small std-only state so it can be unit
//! tested without a socket. `main.rs` only wires it in. Defaults are chosen so a
//! single developer on loopback never notices them; every limit has an env knob:
//!
//! | env | default | meaning |
//! |---|---|---|
//! | `MC_SPINE_ALLOW_PUBLIC_BIND` | off | required to bind any non-loopback address |
//! | `MC_SPINE_MAX_CONN_PER_IP` | 4 (non-loopback only) | concurrent connections per remote IP, `0` = unlimited; when set explicitly it also applies to loopback |
//! | `MC_SPINE_LOGIN_TIMEOUT_SECS` | 15 | wall-clock budget for handshake + login + configuration |
//! | `MC_SPINE_IDLE_TIMEOUT_SECS` | 60 | play state: no complete packet for this long drops the client |
//! | `MC_SPINE_ALLOWLIST` | unset (off) | comma/space separated player names and/or UUIDs permitted to log in |
//! | `MC_SPINE_RCON_MAX_FAILURES` | 5 | failed RCON passwords from one IP before a lockout |
//! | `MC_SPINE_RCON_LOCKOUT_SECS` | 30 | first lockout length; doubles per further failure, capped at 15 min |
//! | `MC_SPINE_BACKUP_INTERVAL_SECS` | 600 | minimum age before an older world backup generation is rotated |

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::{Duration, Instant};

// ----------------------------------------------------------- protocol codecs

/// Login packet 0x03 payload; send before enabling compressed framing.
pub(crate) fn set_compression(threshold: i32) -> Vec<u8> {
    let mut payload = Vec::new();
    crate::write_varint(&mut payload, i64::from(threshold));
    payload
}

/// Frame a packet (including its packet ID) after compression is enabled.
pub(crate) fn encode_compressed_frame(raw: &[u8], threshold: usize) -> Vec<u8> {
    use std::io::Write;

    let mut body = Vec::new();
    if raw.len() < threshold {
        crate::write_varint(&mut body, 0);
        body.extend_from_slice(raw);
    } else {
        crate::write_varint(&mut body, raw.len() as i64);
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder
            .write_all(raw)
            .expect("writing to a Vec cannot fail");
        body.extend_from_slice(&encoder.finish().expect("writing to a Vec cannot fail"));
    }
    let mut frame = Vec::new();
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
    use flate2::{Decompress, FlushDecompress, Status};

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
    let mut decoder = Decompress::new(true);
    let mut decoded = vec![0; length + 1];
    let status = decoder
        .decompress(body, &mut decoded, FlushDecompress::Finish)
        .map_err(|e| format!("invalid zlib data: {e}"))?;
    if status != Status::StreamEnd
        || decoder.total_out() != length as u64
        || decoder.total_in() != body.len() as u64
    {
        return Err("zlib stream does not match the frame length".into());
    }
    decoded.truncate(length);
    Ok(decoded)
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
        assert_eq!(selected.len(), 9);
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
    /// `varint(len)` + `varint(0x27)` + payload — the exact bytes an
    /// uncompressed client receives.
    plain: Arc<Vec<u8>>,
    /// `encode_compressed_frame` of the same body, built on first request for
    /// the negotiated threshold (uniform per server: one env knob).
    compressed: Option<(usize, Arc<Vec<u8>>)>,
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

/// Entries retained; each holds a full chunk frame plus (at most) its
/// compressed form.
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
    ) -> Option<(Arc<Vec<u8>>, Vec<((i32, i32, i32), u8)>)> {
        self.sync_generation(generation);
        let entry = self.map.get_mut(&key)?;
        if entry.revision != revision {
            return None;
        }
        let frame = match compression {
            None => entry.plain.clone(),
            Some(threshold) => match &entry.compressed {
                Some((for_threshold, frame)) if *for_threshold == threshold => frame.clone(),
                _ => {
                    let frame = Arc::new(encode_compressed_frame(
                        body_of_frame(&entry.plain),
                        threshold,
                    ));
                    entry.compressed = Some((threshold, frame.clone()));
                    frame
                }
            },
        };
        Some((frame, entry.own_emitters.clone()))
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
            plain: Arc::new(plain_frame(&chunk_packet_body(&payload))),
            compressed: None,
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
    pub(crate) fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
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
    std::env::var("MC_SPINE_ALLOW_PUBLIC_BIND").is_ok_and(|v| truthy(&v))
}

/// `Ok` for loopback, or for anything when the operator opted in explicitly.
pub(crate) fn check_bind(what: &str, addr: &str, allow_public: bool) -> Result<(), String> {
    if allow_public || is_loopback_bind(addr) {
        return Ok(());
    }
    Err(format!(
        "refusing to bind {what} to non-loopback address {addr}: this server uses offline-mode \
         identities (no authentication) and plaintext traffic. Set MC_SPINE_ALLOW_PUBLIC_BIND=1 \
         only behind a firewall/private network you control."
    ))
}

// ------------------------------------------------------------ timeouts / knobs

pub(crate) fn login_timeout() -> Duration {
    Duration::from_secs(
        env_parse("MC_SPINE_LOGIN_TIMEOUT_SECS")
            .unwrap_or(15)
            .max(1),
    )
}

static IDLE_TIMEOUT_SECS: OnceLock<AtomicU64> = OnceLock::new();

fn idle_timeout_secs() -> &'static AtomicU64 {
    IDLE_TIMEOUT_SECS.get_or_init(|| {
        AtomicU64::new(env_parse("MC_SPINE_IDLE_TIMEOUT_SECS").unwrap_or(60).max(1))
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
    Duration::from_secs(env_parse("MC_SPINE_BACKUP_INTERVAL_SECS").unwrap_or(600))
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
/// `MC_SPINE_MAX_CONN_PER_IP` explicitly applies it everywhere. `0` = unlimited.
pub(crate) fn per_ip_limit(ip: IpAddr) -> usize {
    per_ip_limit_with(ip, env_parse::<usize>("MC_SPINE_MAX_CONN_PER_IP"))
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

/// The configured allowlist (`MC_SPINE_ALLOWLIST`), read once. `None` = off.
pub(crate) fn allowlist() -> Option<&'static Allowlist> {
    static LIST: std::sync::OnceLock<Option<Allowlist>> = std::sync::OnceLock::new();
    LIST.get_or_init(|| {
        let list = Allowlist::parse(&std::env::var("MC_SPINE_ALLOWLIST").unwrap_or_default());
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
    env_parse("MC_SPINE_RCON_MAX_FAILURES").unwrap_or(5)
}

pub(crate) fn rcon_lockout() -> Duration {
    Duration::from_secs(env_parse("MC_SPINE_RCON_LOCKOUT_SECS").unwrap_or(30).max(1))
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
            assert!(e.contains("MC_SPINE_ALLOW_PUBLIC_BIND"), "{bad}: {e}");
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
        let (c3, _) = cache.frame(10, key, 41, Some(64)).unwrap();
        assert!(
            !Arc::ptr_eq(&c1, &c3),
            "each threshold builds its own compressed frame"
        );
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
}
