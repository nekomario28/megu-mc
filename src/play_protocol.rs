// ---------- varint / wire ----------
fn read_varint(b: &mut &[u8]) -> Option<i64> {
    let mut v = 0i64;
    let mut i = 0;
    loop {
        if b.is_empty() || i > 9 {
            return None;
        }
        let byte = b[0];
        *b = &b[1..];
        v |= ((byte & 0x7f) as i64) << (7 * i);
        if byte & 0x80 == 0 {
            return Some(v);
        }
        i += 1;
    }
}
/// Protocol VarInt is at most five bytes; the save reader also supports wider values.
fn read_protocol_i32(b: &mut &[u8]) -> Option<i32> {
    let before = b.len();
    let value = read_varint(b)?;
    if before - b.len() > 5 || value > u32::MAX as i64 {
        return None;
    }
    Some(value as u32 as i32)
}
fn write_varint(out: &mut Vec<u8>, v: i64) {
    // i32-range negatives encode as VarInt (5-byte u32 LEB128); wider as VarLong.
    // (arithmetic `>>=` on a negative i64 loops forever -> unbounded Vec growth)
    let mut v = if v < 0 && v >= i32::MIN as i64 {
        v as u32 as u64
    } else {
        v as u64
    };
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80
        }
        out.push(b);
        if v == 0 {
            break;
        }
    }
}
fn write_str(out: &mut Vec<u8>, s: &str) {
    write_varint(out, s.len() as i64);
    out.extend_from_slice(s.as_bytes());
}
fn write_pos(out: &mut Vec<u8>, x: i32, y: i32, z: i32) {
    let v = ((x as i64 & 0x3ffffff) << 38) | ((z as i64 & 0x3ffffff) << 12) | (y as i64 & 0xfff);
    out.extend_from_slice(&v.to_be_bytes());
}
fn read_pos(b: &[u8]) -> (i32, i32, i32) {
    let v = i64::from_be_bytes(b[..8].try_into().unwrap());
    let x = (v >> 38) as i32;
    let z = ((v >> 12) & 0x3ffffff) as i32;
    let y = (v & 0xfff) as i32;
    ((x << 6) >> 6, (y << 20) >> 20, (z << 6) >> 6) // sign-extend, (x,y,z) order
}

// A single wall-clock deadline covers every byte of pre-play handshaking.
// Socket read timeouts alone can be bypassed by sending one byte at a time.
fn read_before(s: &mut TcpStream, mut out: &mut [u8], deadline: Instant) -> Option<()> {
    while !out.is_empty() {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        s.set_read_timeout(Some(remaining)).ok()?;
        let n = s.read(out).ok()?;
        if n == 0 {
            return None;
        }
        if let Some(cipher) = conn_cipher(s) {
            cipher.rx.lock().unwrap().decrypt(&mut out[..n]);
        }
        out = &mut out[n..];
    }
    Some(())
}
fn read_packet(s: &mut TcpStream, deadline: Instant) -> Option<(i64, Vec<u8>)> {
    read_login_frame(s, deadline, None)
}

fn read_login_frame(
    s: &mut TcpStream,
    deadline: Instant,
    compression: Option<usize>,
) -> Option<(i64, Vec<u8>)> {
    const MAX_LOGIN_FRAME: usize = 64 * 1024;
    let mut len = 0u64;
    let mut done = false;
    for i in 0..3 {
        let mut b = [0u8; 1];
        read_before(s, &mut b, deadline)?;
        len |= ((b[0] & 0x7f) as u64) << (7 * i);
        if b[0] & 0x80 == 0 {
            done = true;
            break;
        }
    }
    // Pre-play packets are small; reject large declared allocations.
    if !done || len == 0 || len > MAX_LOGIN_FRAME as u64 {
        let _ = s.shutdown(std::net::Shutdown::Both);
        return None;
    }
    let mut body = vec![0u8; len as usize];
    read_before(s, &mut body, deadline)?;
    decode_packet(body, compression, MAX_LOGIN_FRAME)
}

fn decode_packet(
    body: Vec<u8>,
    compression: Option<usize>,
    max_size: usize,
) -> Option<(i64, Vec<u8>)> {
    let body = match compression {
        Some(threshold) => {
            connection_limits::decode_compressed_frame(&body, threshold, max_size).ok()?
        }
        None => body,
    };
    let mut payload = body.as_slice();
    let id = read_protocol_i32(&mut payload)?;
    (id >= 0).then(|| (i64::from(id), payload.to_vec()))
}

/// The ceiling an *inflated* play frame may occupy, from `/megu policy set
/// max_frame_mib <2..=32>`. The wire prefix still cannot describe more than
/// vanilla's three-byte VarInt bound — this governs the decompressed payload
/// only, so raising it admits modded frames whose compressed bodies stay
/// small. The default `2` lands exactly on `ingress_guard::MAX_PLAY_FRAME`.
fn live_play_frame_bound() -> usize {
    ((live_server_policy()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .max_frame_mib as usize)
        << 20)
        - 1
}
// Buffered frame reader for the consume loop: a mid-frame timeout must NOT
// drop already-read bytes. Dropped length bytes desync the stream -> garbage
// length fields -> repeated <=16MiB allocs -> allocator arena churn -> the OOM.
struct RxBuf {
    buf: std::collections::VecDeque<u8>,
    compression: Option<usize>,
}
enum RxOut {
    Closed,
    Idle,
    Frame(i64, Vec<u8>),
}
impl RxBuf {
    fn new(compression: Option<usize>) -> Self {
        RxBuf {
            buf: std::collections::VecDeque::new(),
            compression,
        }
    }
    fn try_frame(&mut self) -> Option<Option<RxOut>> {
        let n = self.buf.len();
        let mut len = 0i64;
        let mut i = 0usize;
        loop {
            if i >= n {
                return Some(None);
            }
            let b = self.buf[i];
            len |= ((b & 0x7f) as i64) << (7 * i);
            i += 1;
            if b & 0x80 == 0 {
                break;
            }
            if i == 3 {
                return None;
            }
        }
        // vanilla's Varint21FrameDecoder bound: a three-byte length prefix cannot
        // describe more, so a larger frame is not something a 1.21.1 client sends.
        if len == 0 || len as usize > ingress_guard::MAX_PLAY_FRAME {
            return None;
        }
        let total = i + len as usize;
        if n < total {
            return Some(None);
        }
        let body: Vec<u8> = self
            .buf
            .iter()
            .skip(i)
            .take(len as usize)
            .copied()
            .collect();
        self.buf.drain(..total);
        let (id, payload) = decode_packet(body, self.compression, live_play_frame_bound())?;
        Some(Some(RxOut::Frame(id, payload)))
    }
    fn next(&mut self, s: &mut TcpStream) -> RxOut {
        match self.try_frame() {
            None => {
                let _ = s.shutdown(std::net::Shutdown::Both);
                return RxOut::Closed;
            }
            Some(Some(f)) => return f,
            Some(None) => {}
        }
        let mut tmp = [0u8; 8192];
        match s.read(&mut tmp) {
            Ok(0) => RxOut::Closed,
            Ok(n) => {
                if let Some(cipher) = conn_cipher(s) {
                    cipher.rx.lock().unwrap().decrypt(&mut tmp[..n]);
                }
                self.buf.extend(&tmp[..n]);
                match self.try_frame() {
                    Some(frame) => frame.unwrap_or(RxOut::Idle),
                    None => {
                        let _ = s.shutdown(std::net::Shutdown::Both);
                        RxOut::Closed
                    }
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                RxOut::Idle
            }
            Err(_) => RxOut::Closed,
        }
    }
}
#[cfg(unix)]
fn socket_key(socket: &TcpStream) -> u64 {
    use std::os::fd::AsRawFd;
    socket.as_raw_fd() as u64
}

#[cfg(windows)]
fn socket_key(socket: &TcpStream) -> u64 {
    use std::os::windows::io::AsRawSocket;
    socket.as_raw_socket() as u64
}

#[derive(Default)]
struct SocketWireState {
    compression: Option<usize>,
    play: Option<PlayWireState>,
    cipher: Option<Arc<ConnCipher>>,
}

/// The AES-128-CFB8 pair one online-mode connection runs under. Registered per
/// socket fd like the rest of `SocketWireState`: the login socket and the
/// `try_clone`d play writer are two fds pointing at one `Arc`, so bytes either
/// side writes advance a single keystream — matching Java's single cipher pair.
struct ConnCipher {
    rx: Mutex<Aes128Cfb8>,
    tx: Mutex<Aes128Cfb8>,
}

/// The connection's cipher if encryption was negotiated on this socket, else
/// `None` — one map lookup per socket call, on top of the lookup `send_packet`
/// already performs.
fn conn_cipher(s: &TcpStream) -> Option<Arc<ConnCipher>> {
    SOCKET_WIRE
        .get()?
        .lock()
        .unwrap()
        .get(&socket_key(s))?
        .cipher
        .clone()
}

/// Arm encryption on `s`'s wire entry with the negotiated shared secret. Called
/// the moment the Encryption Response verifies: every later byte crossing the
/// read/write paths below is en- or decrypted through `conn_cipher`.
fn enable_conn_encryption(s: &TcpStream, secret: &[u8; 16]) {
    let cipher = Arc::new(ConnCipher {
        rx: Mutex::new(Aes128Cfb8::new(secret, secret)),
        tx: Mutex::new(Aes128Cfb8::new(secret, secret)),
    });
    socket_wire()
        .lock()
        .unwrap()
        .entry(socket_key(s))
        .or_default()
        .cipher = Some(cipher);
}

#[derive(Default)]
struct PlayWireState {
    poses: HashMap<i32, TrackedWirePose>,
    chunk_delivery: connection_limits::ChunkDelivery,
}

struct TrackedWirePose {
    pose: EntityWirePose,
    interval: u64,
    last_sent_tick: Option<u64>,
}

#[derive(Clone, Copy, PartialEq)]
struct EntityWirePose {
    pos: [f64; 3],
    yaw: u8,
    pitch: u8,
    on_ground: Option<bool>,
}

impl EntityWirePose {
    fn read(fields: &[u8], spawn: bool) -> Option<Self> {
        let pos = [
            f64::from_be_bytes(fields.get(..8)?.try_into().ok()?),
            f64::from_be_bytes(fields.get(8..16)?.try_into().ok()?),
            f64::from_be_bytes(fields.get(16..24)?.try_into().ok()?),
        ];
        let [a, b, tail] = fields.get(24..27)?.try_into().ok()?;
        let (yaw, pitch, on_ground) = if spawn {
            (b, a, None)
        } else {
            (a, b, Some(tail != 0))
        };
        Some(Self {
            pos,
            yaw,
            pitch,
            on_ground,
        })
    }

    fn relative_packet(self, eid: i32, next: Self) -> Option<(i64, Vec<u8>)> {
        let delta = connection_limits::relative_move_delta(self.pos, next.pos)?;
        let rotation = (next.yaw, next.pitch);
        let rotated = (self.yaw, self.pitch) != rotation;
        let grounded = next.on_ground?;
        if delta == [0; 3] && rotated {
            Some((
                0x30,
                connection_limits::entity_rotation(eid, next.yaw, next.pitch, grounded),
            ))
        } else {
            Some((
                if rotated { 0x2f } else { 0x2e },
                connection_limits::entity_relative_move(
                    eid,
                    delta,
                    rotated.then_some(rotation),
                    grounded,
                ),
            ))
        }
    }
}

impl PlayWireState {
    fn remember_spawn(&mut self, mut payload: &[u8]) {
        let Some(eid) = read_protocol_i32(&mut payload) else {
            return;
        };
        let Some(mut fields) = payload.get(16..) else {
            return; // UUID
        };
        let Some(etype) = read_protocol_i32(&mut fields) else {
            return;
        };
        let Some(pose) = EntityWirePose::read(fields, true) else {
            return;
        };
        let interval = entity_spec_by_id(i64::from(etype))
            .map_or(0, |spec| u64::from(entity_update_interval_ticks(spec.name)));
        self.poses.insert(
            eid,
            TrackedWirePose {
                pose,
                interval,
                last_sent_tick: None,
            },
        );
    }

    fn update_packet(&mut self, id: &mut i64, payload: &mut Vec<u8>, tick: u64) -> bool {
        let mut fields = payload.as_slice();
        match *id {
            0x70 => {
                let Some(eid) = read_protocol_i32(&mut fields) else {
                    return true;
                };
                let Some(previous) = self.poses.get_mut(&eid) else {
                    return true;
                };
                let Some(next) = EntityWirePose::read(fields, false) else {
                    return true;
                };
                // These producers also send teleports and impulse-driven movement.
                // Only a repeat of the last sent pose is safe to pace by type here.
                if previous.pose == next
                    && previous
                        .last_sent_tick
                        .is_some_and(|sent| tick.wrapping_sub(sent) < previous.interval)
                {
                    return false;
                }
                if let Some(packet) = previous.pose.relative_packet(eid, next) {
                    (*id, *payload) = packet;
                }
                previous.pose = next;
                previous.last_sent_tick = Some(tick);
            }
            0x42 => {
                let Some(count) = read_protocol_i32(&mut fields) else {
                    return true;
                };
                for _ in 0..count {
                    let Some(eid) = read_protocol_i32(&mut fields) else {
                        return true;
                    };
                    self.poses.remove(&eid);
                }
            }
            0x47 => {
                self.poses.clear();
                self.chunk_delivery.reset_window();
            }
            _ => {}
        }
        true
    }
}

// Existing packet producers share TcpStream, including the cloned Play writer.
// Store framing against each owned handle until its owner drops, before the OS
// can reuse the handle. The registry lock is released before compression or I/O.
static SOCKET_WIRE: OnceLock<Mutex<HashMap<u64, SocketWireState>>> = OnceLock::new();

fn socket_wire() -> &'static Mutex<HashMap<u64, SocketWireState>> {
    SOCKET_WIRE.get_or_init(Mutex::default)
}

fn with_play_wire<T>(socket: &TcpStream, f: impl FnOnce(&mut PlayWireState) -> T) -> Option<T> {
    let mut states = SOCKET_WIRE.get()?.lock().unwrap();
    states.get_mut(&socket_key(socket))?.play.as_mut().map(f)
}

fn unregister_socket_wire(key: u64) {
    if let Some(states) = SOCKET_WIRE.get() {
        states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
    }
}

struct LoginCompression(u64);

impl std::ops::Drop for LoginCompression {
    fn drop(&mut self) {
        unregister_socket_wire(self.0);
    }
}

/// Same drop-unregister discipline for the cipher `enable_conn_encryption`
/// arms on the login socket's fd. Without it the entry outlives the connection
/// and a later connection reusing the fd inherits a dead keystream.
struct SocketCipherGuard(u64);

impl std::ops::Drop for SocketCipherGuard {
    fn drop(&mut self) {
        unregister_socket_wire(self.0);
    }
}

fn negotiate_compression(
    s: &mut TcpStream,
    threshold: i32,
) -> std::io::Result<Option<LoginCompression>> {
    let Ok(size) = usize::try_from(threshold) else {
        return Ok(None);
    };
    send_packet(s, 0x03, connection_limits::set_compression(threshold))?;
    let key = socket_key(s);
    let mut states = socket_wire().lock().unwrap();
    // Merge, not overwrite: online-mode login arms a cipher on this socket
    // before compression is negotiated.
    states.entry(key).or_default().compression = Some(size);
    Ok(Some(LoginCompression(key)))
}

fn send_packet(s: &mut TcpStream, mut id: i64, mut payload: Vec<u8>) -> std::io::Result<()> {
    if id == 0x5a {
        // SetEntityMotion: whatever the server (or a mod acting through it) launches a
        // player with is movement the anti-cheat must allow for.
        let mut body = payload.as_slice();
        if let Some(eid) = read_varint(&mut body) {
            if body.len() >= 6 {
                let axis = |i: usize| f64::from(i16::from_be_bytes([body[i], body[i + 1]])) / 8000.0;
                ac_note_server_velocity(eid as i32, [axis(0), axis(2), axis(4)]);
            }
        }
    }
    let mut send = true;
    let compression = SOCKET_WIRE.get().and_then(|states| {
        let mut states = states.lock().unwrap();
        let state = states.get_mut(&socket_key(s))?;
        if let Some(play) = &mut state.play {
            send = play.update_packet(&mut id, &mut payload, TICK_IDX.load(Ordering::Relaxed));
        }
        state.compression
    });
    if !send {
        return Ok(());
    }
    let mut body = Vec::new();
    write_varint(&mut body, id);
    body.extend_from_slice(&payload);
    write_frame(s, &body, compression)
}

fn write_frame(s: &mut TcpStream, body: &[u8], compression: Option<usize>) -> std::io::Result<()> {
    let mut frame = match compression {
        Some(threshold) => connection_limits::encode_compressed_frame(body, threshold),
        None => {
            let mut frame = Vec::new();
            write_varint(&mut frame, body.len() as i64);
            frame.extend_from_slice(body);
            frame
        }
    };
    if let Some(cipher) = conn_cipher(s) {
        cipher.tx.lock().unwrap().encrypt(&mut frame);
    }
    let result = s.write_all(&frame);
    if result.is_err() {
        // A partial write invalidates both the frame boundary and entity baselines.
        let _ = s.shutdown(std::net::Shutdown::Both);
    }
    result
}

/// `write_frame` for an already-framed packet. `SharedBroadcastFrame` fans the
/// same `Arc<[u8]>` out to every recipient, so the bytes here are borrowed: a
/// plain connection writes them with no copy at all, while a ciphered
/// connection encrypts a scratch copy rather than mutating the shared
/// allocation the other recipients still have to send.
fn write_raw_frame(s: &mut TcpStream, frame: &[u8]) -> std::io::Result<()> {
    let result = if let Some(cipher) = conn_cipher(s) {
        let mut bytes = frame.to_vec();
        cipher
            .tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .encrypt(&mut bytes);
        s.write_all(&bytes)
    } else {
        s.write_all(frame)
    };
    if result.is_err() {
        // A partial write invalidates both the frame boundary and entity baselines.
        let _ = s.shutdown(std::net::Shutdown::Both);
    }
    result
}

/// One broadcast's reusable wire form. The first `(id, payload)` `mk` produces
/// is framed once by the `SharedBroadcastFrame`; every later recipient `mk`
/// hands the same bytes to is served from it, so the fanout encodes and — for
/// compressed clients — compresses once instead of once per client. A
/// recipient whose `mk` returns different bytes reframes, which keeps
/// per-client producers byte-exact.
struct BroadcastFrameCache {
    id: i64,
    payload: Box<[u8]>,
    frame: connection_limits::SharedBroadcastFrame,
}

/// The shared frame for `(id, payload)`, building the cache entry on the first
/// call and whenever `mk` produced different bytes than the last recipient.
fn broadcast_frame<'c>(
    cache: &'c mut Option<BroadcastFrameCache>,
    id: i64,
    payload: &[u8],
) -> &'c mut connection_limits::SharedBroadcastFrame {
    let hit = cache
        .as_ref()
        .is_some_and(|c| c.id == id && c.payload.as_ref() == payload);
    if !hit {
        *cache = Some(BroadcastFrameCache {
            id,
            payload: payload.into(),
            frame: connection_limits::SharedBroadcastFrame::new(id, payload),
        });
    }
    &mut cache.as_mut().expect("populated above").frame
}

/// `send_packet` for the broadcast loops: the per-recipient work — the
/// anti-cheat note and the pose-tracking `update_packet` gate — still runs on
/// every client exactly as before, so a gate suppression or a per-client
/// rewrite (a `0x70` teleport becoming a relative move this one recipient
/// tracks differently) stays byte-identical to the old per-client send. Only
/// the framing changes: a packet that crosses the gate unmodified writes the
/// broadcast's shared `SharedBroadcastFrame`, encoding and compressing once
/// for the whole fanout rather than once per recipient.
fn send_broadcast_frame(
    s: &mut TcpStream,
    cache: &mut Option<BroadcastFrameCache>,
    mut id: i64,
    payload: &[u8],
) -> std::io::Result<()> {
    if id == 0x5a {
        // SetEntityMotion: whatever the server (or a mod acting through it) launches a
        // player with is movement the anti-cheat must allow for.
        let mut body = payload;
        if let Some(eid) = read_varint(&mut body) {
            if body.len() >= 6 {
                let axis = |i: usize| f64::from(i16::from_be_bytes([body[i], body[i + 1]])) / 8000.0;
                ac_note_server_velocity(eid as i32, [axis(0), axis(2), axis(4)]);
            }
        }
    }
    let mut send = true;
    let mut rewritten: Option<Vec<u8>> = None;
    let compression = SOCKET_WIRE.get().and_then(|states| {
        let mut states = states.lock().unwrap_or_else(|e| e.into_inner());
        let state = states.get_mut(&socket_key(s))?;
        if let Some(play) = &mut state.play {
            let mut wire_id = id;
            let mut scratch = payload.to_vec();
            send = play.update_packet(
                &mut wire_id,
                &mut scratch,
                TICK_IDX.load(Ordering::Relaxed),
            );
            if wire_id != id || scratch.as_slice() != payload {
                id = wire_id;
                rewritten = Some(scratch);
            }
        }
        state.compression
    });
    if !send {
        return Ok(());
    }
    if let Some(payload) = rewritten {
        let mut body = Vec::with_capacity(payload.len() + 5);
        write_varint(&mut body, id);
        body.extend_from_slice(&payload);
        return write_frame(s, &body, compression);
    }
    let frame = broadcast_frame(cache, id, payload).frame_for(compression);
    write_raw_frame(s, frame.as_ref())
}

/// A START_DESTROY_BLOCK the client has not cancelled. Vanilla survival mining
/// advances on the server (`ServerPlayerGameMode.tick` accumulates destroy
/// progress and breaks the block when it reaches the dig duration); the finish
/// packet a client sends only predicts the same event, so the stored duration
/// here is the completion trigger. `state` pins the block the dig was computed
/// for — if the cell changed underneath, the dig is abandoned instead.
#[derive(Clone, Copy)]
struct PendingDig {
    pos: DimBlockPos,
    state: u32,
    started: Instant,
    seconds: f32,
}

struct Client {
    eid: i32,
    uuid: [u8; 16],
    name: String,
    player_attack_damage_id: Option<i32>,
    pos: Mutex<(f64, f64, f64, f32, f32, bool)>,
    teleport: Mutex<connection_limits::TeleportSync>,
    hp: Mutex<f32>,
    sock: Mutex<TcpStream>,
    inv: Mutex<HashMap<i16, Stack>>,
    cursor: Mutex<Stack>,
    drag: Mutex<(i32, Vec<i16>)>, // (button, painted window slots) for click-mode 5
    open_chest: Mutex<Option<(ContainerKey, Option<ContainerKey>)>>, // (key, double-chest partner)
    chest_sync: Mutex<Option<ChestSyncState>>,
    open_furnace: Mutex<Option<DimBlockPos>>,
    craft: Mutex<HashMap<i16, Stack>>,
    craft9: Mutex<HashMap<i16, Stack>>,
    open_table: Mutex<bool>,
    // Legacy fields still initialized by the synthetic client fixtures.
    #[cfg(test)]
    #[allow(dead_code)]
    open_merchant: Mutex<bool>,
    #[cfg(test)]
    #[allow(dead_code)]
    trade_sel: Mutex<i32>,
    #[cfg(test)]
    #[allow(dead_code)]
    merchant_in: Mutex<[Stack; 2]>,
    bow_draw: Mutex<Option<Instant>>,
    dig: Mutex<Option<PendingDig>>,
    eat_start: Mutex<Option<Instant>>,
    blocking: Mutex<bool>,
    sprinting: Mutex<bool>,
    sneaking: Mutex<bool>,
    last_melee: Mutex<Instant>,
    fall_fly: Mutex<bool>,
    invulnerable: Mutex<i32>, // vanilla LivingEntity.invulnerableTime, in server ticks
    last_hurt: Mutex<f32>,    // vanilla LivingEntity.lastHurt: the last accepted damage
    shield_cd: Mutex<Instant>,
    absorb: Mutex<f32>,
    pearl_cd: Mutex<Instant>,
    xp: Mutex<i64>,
    skin: Mutex<Option<(String, String)>>, // textures (value, signature) resolved from Mojang
    seen: Mutex<HashSet<i32>>,             // entity ids currently spawned for this client
    chunks: Mutex<HashSet<(i32, i32)>>,    // chunks the client currently holds
    chunk_ack: Mutex<ChunkFeedback>,       // chunk batch pacing: the client's requested rate
    gen_sources: Mutex<GenSources>, // generated light emitters per held chunk, for the light pass
    gm: Mutex<u8>,                  // 0=survival 1=creative 3=spectator
    fall_from: Mutex<f64>,
    food: Mutex<i32>,
    air: Mutex<i32>,
    armor_pts: Mutex<i32>,
    sat: Mutex<f32>,
    walk_acc: Mutex<f64>,
    lastp: Mutex<(f64, f64, f64)>,
    spawn_pt: Mutex<Option<(f64, f64, f64, f32)>>,
    // P2-3-S4: a player lives in one dimension at a time. Every dimension-scoped
    // audience filter (chunks, block deltas, entity visibility) reads this, and a
    // 0x47 Respawn flips it together with the chunk window. The saved spawn point
    // carries its own dimension so death respawn returns the player to the right
    // world even when they died in another one.
    dim: Mutex<DimId>,
    // The client's effective view distance, mirrored from the connection so commands and
    // portals can rebuild a window of the same size without a stream reference.
    view: Mutex<i32>,
    spawn_dim: Mutex<DimId>,
    // Set while the client is standing inside a portal block. Cleared on arrival
    // in the destination so the freshly-materialised return portal does not bounce
    // the player straight back (a per-player, not per-block, guard).
    in_portal: Mutex<bool>,
    portal_at: Mutex<Option<Instant>>,
    held: Mutex<i16>,
    effs: Mutex<Vec<Eff>>,
    // The `mc-spine:lod_v1` opt-in from the client's `minecraft:register`
    // payload. Vanilla clients never set it and must never receive a `lod_v1`
    // packet, so the flag is checked before every channel write.
    lod_v1_enabled: AtomicBool,
    // Anti-xray lane hysteresis: `true` while the player is underground enough
    // to see real chunks, `false` while they get the surface-occluded lane.
    // Flipped by `update_underground_lane`, which re-sends held chunks.
    underground_revealed: AtomicBool,
    // `Some` while the player lies in a bed; `sleep_tick` ages it and clears it
    // on wake, damage or a completed night skip.
    sleeping: Mutex<Option<Sleeping>>,
    // Window-0/container stateId counter: every server-initiated full-content
    // resync bumps it so a stale client echo on 0x0e can be detected.
    inv_state_id: AtomicI32,
}

// Per-player bed occupancy: the bed position, game ticks slept so far and the
// `last_hurt` watermark at sleep entry so damage wakes the player.
#[derive(Clone, Copy)]
struct Sleeping {
    bed: (i32, i32, i32),
    ticks: u32,
    last_hurt: f32,
}

impl Client {
    fn enter_play_wire(
        &self,
        compression: Option<usize>,
        chunk_delivery: connection_limits::ChunkDelivery,
    ) {
        let key = socket_key(&self.sock.lock().unwrap());
        let mut states = socket_wire().lock().unwrap();
        // Merge, not overwrite: the encryption handshake may have armed a
        // cipher on this writer fd before the play state existed.
        let state = states.entry(key).or_default();
        state.compression = compression;
        state.play = Some(PlayWireState {
            chunk_delivery,
            ..PlayWireState::default()
        });
    }

    fn is_spectator(&self) -> bool {
        *self.gm.lock().unwrap() == 3
    }
    fn touch_aabb(&self) -> EntityAabb {
        let (px, py, pz, ..) = *self.pos.lock().unwrap();
        let sneaking = *self.sneaking.lock().unwrap();
        let fall_fly = *self.fall_fly.lock().unwrap();
        player_touch_aabb(px, py, pz, sneaking, fall_fly)
    }
}

impl std::ops::Drop for Client {
    fn drop(&mut self) {
        ac_forget(self.eid);
        let socket = self.sock.get_mut().unwrap_or_else(|e| e.into_inner());
        unregister_socket_wire(socket_key(socket));
    }
}
static CLIENTS: OnceLock<Mutex<Vec<Arc<Client>>>> = OnceLock::new();
const PLAYER_ENTITY_TYPE: i64 = 128; // minecraft:player in 1.21.1
static NEXT_EID: AtomicI32 = AtomicI32::new(1000);
fn clients() -> &'static Mutex<Vec<Arc<Client>>> {
    CLIENTS.get_or_init(|| Mutex::new(Vec::new()))
}
enum AngleRounding {
    Floor,
    Truncate,
}

// 1.21.1 packet constructors have distinct rounding policies. Narrow through
// Java's int boundary before the wrapping byte conversion, including overflow.
fn angle_byte(scaled: f64, rounding: AngleRounding) -> i8 {
    let value = scaled as i32; // Java's saturating floating-point -> int conversion.
    let value = if matches!(rounding, AngleRounding::Floor) && scaled < f64::from(value) {
        value.wrapping_sub(1) // Mth.floor also wraps below Integer.MIN_VALUE.
    } else {
        value
    };
    value as i8 // Java int -> byte wraps; a direct Rust float -> i8 saturates.
}
fn angle(deg: f32) -> i8 {
    angle_byte(f64::from(deg * 256.0 / 360.0), AngleRounding::Floor)
}
fn teleport_angle(deg: f32) -> i8 {
    // ClientboundTeleportEntityPacket (afw) uses f2i, not Mth.floor.
    angle_byte(f64::from(deg * 256.0 / 360.0), AngleRounding::Truncate)
}
fn spawn_angles(out: &mut Vec<u8>, yaw: f32, pitch: f32) {
    // ClientboundAddEntityPacket (abv): body uses f32, head uses f64.
    out.extend_from_slice(&[
        angle(pitch) as u8,
        angle(yaw) as u8,
        angle_byte(f64::from(yaw) * 256.0 / 360.0, AngleRounding::Floor) as u8,
    ]);
}

// Bounded: one entry per distinct name an (unauthenticated) client logs in
// with would otherwise grow forever, and each miss costs two outbound requests.
const SKIN_CACHE_CAP: usize = 1024;
const SKIN_TTL_HIT: Duration = Duration::from_secs(60 * 60);
const SKIN_TTL_MISS: Duration = Duration::from_secs(5 * 60);
/// Concurrent outbound lookups (and lookup threads); a login whose name is not
/// cached while this many are in flight simply goes without a skin.
const SKIN_LOOKUPS_MAX: usize = 4;
static SKIN_INFLIGHT: AtomicUsize = AtomicUsize::new(0);
type SkinEntry = (Instant, Option<(String, String)>);
static SKIN_CACHE: OnceLock<Mutex<HashMap<String, SkinEntry>>> = OnceLock::new();
fn skin_cache() -> &'static Mutex<HashMap<String, SkinEntry>> {
    SKIN_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}
fn skin_entry_fresh(e: &SkinEntry, now: Instant) -> bool {
    let ttl = if e.1.is_some() {
        SKIN_TTL_HIT
    } else {
        SKIN_TTL_MISS
    };
    now.saturating_duration_since(e.0) < ttl
}
/// Fresh cached result for `key` (outer `None` = not cached or expired).
fn skin_cache_get(
    c: &HashMap<String, SkinEntry>,
    key: &str,
    now: Instant,
) -> Option<Option<(String, String)>> {
    c.get(key)
        .filter(|e| skin_entry_fresh(e, now))
        .map(|e| e.1.clone())
}
/// Insert, keeping at most `cap` entries: expired ones go first, then the oldest.
fn skin_cache_put(
    c: &mut HashMap<String, SkinEntry>,
    key: String,
    res: Option<(String, String)>,
    now: Instant,
    cap: usize,
) {
    if c.len() >= cap && !c.contains_key(&key) {
        c.retain(|_, e| skin_entry_fresh(e, now));
        while c.len() >= cap {
            let Some(oldest) = c.iter().min_by_key(|(_, e)| e.0).map(|(k, _)| k.clone()) else {
                break;
            };
            c.remove(&oldest);
        }
    }
    c.insert(key, (now, res));
}
fn load_skin_properties(name: &str) -> Option<(String, String)> {
    // offline-mode skin resolution: name -> uuid -> session profile textures
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(6)))
        .build()
        .into();
    let prof = agent
        .get(format!(
            "https://api.mojang.com/users/profiles/minecraft/{}",
            name
        ))
        .call()
        .ok()?
        .body_mut()
        .read_to_string()
        .ok()?;
    let id = jstr(&prof, "id")?.to_string();
    let full = agent
        .get(format!(
            "https://sessionserver.mojang.com/session/minecraft/profile/{}?unsigned=false",
            id
        ))
        .call()
        .ok()?
        .body_mut()
        .read_to_string()
        .ok()?;
    let value = jstr(&full, "value")?.to_string();
    let signature = jstr(&full, "signature")?.to_string();
    Some((value, signature))
}
/// Give `t` its resolved textures and re-announce it so connected viewers see them.
fn apply_skin(t: &Arc<Client>, res: Option<(String, String)>) {
    if let Some((v, sig)) = res {
        *t.skin.lock().unwrap() = Some((v.clone(), sig.clone()));
        // re-add player info so already-connected viewers pick up the skin
        let cs = clients().lock().unwrap();
        if !cs.iter().any(|c| Arc::ptr_eq(c, t)) {
            return;
        }
        let pi = player_info_add(t);
        for c in cs.iter() {
            let _ = send_packet(&mut c.sock.lock().unwrap(), 0x3e, pi.clone());
        }
    }
}
struct SkinLookupSlot;
impl SkinLookupSlot {
    fn try_acquire() -> Option<Self> {
        if SKIN_INFLIGHT.fetch_add(1, Ordering::SeqCst) < SKIN_LOOKUPS_MAX {
            Some(SkinLookupSlot)
        } else {
            SKIN_INFLIGHT.fetch_sub(1, Ordering::SeqCst);
            None
        }
    }
}
impl std::ops::Drop for SkinLookupSlot {
    fn drop(&mut self) {
        SKIN_INFLIGHT.fetch_sub(1, Ordering::SeqCst);
    }
}
fn kick_skin(name: String, target: &Arc<Client>) {
    // Mojang names are case-insensitive; one cache entry per name, not per spelling.
    let key = name.to_ascii_lowercase();
    let hit = skin_cache_get(&skin_cache().lock().unwrap(), &key, Instant::now());
    if let Some(res) = hit {
        apply_skin(target, res);
        return;
    }
    // Not cached: at most SKIN_LOOKUPS_MAX lookups (and threads) at a time.
    let Some(slot) = SkinLookupSlot::try_acquire() else {
        return;
    };
    let t = target.clone();
    thread::spawn(move || {
        let _slot = slot;
        let r = load_skin_properties(&name);
        skin_cache_put(
            &mut skin_cache().lock().unwrap(),
            key,
            r.clone(),
            Instant::now(),
            SKIN_CACHE_CAP,
        );
        apply_skin(&t, r);
    });
}
fn player_info_add(c: &Client) -> Vec<u8> {
    // actions: add_player|update_game_mode|update_listed|update_latency = 0x1d
    let mut p = vec![0x1du8];
    write_varint(&mut p, 1);
    p.extend_from_slice(&c.uuid);
    write_str(&mut p, &c.name);
    let skin = c.skin.lock().unwrap().clone();
    if let Some((v, sig)) = skin {
        write_varint(&mut p, 1);
        write_str(&mut p, "textures");
        write_str(&mut p, &v);
        p.push(1); // signed
        write_str(&mut p, &sig);
    } else {
        write_varint(&mut p, 0); // properties
    }
    write_varint(&mut p, 1); // gamemode creative
    write_varint(&mut p, 1); // listed
    write_varint(&mut p, 0); // latency
    p
}
fn spawn_player(c: &Client) -> Vec<u8> {
    let (x, y, z, yaw, pitch, _g) = *c.pos.lock().unwrap();
    let mut p = Vec::new();
    write_varint(&mut p, c.eid as i64);
    p.extend_from_slice(&c.uuid);
    write_varint(&mut p, PLAYER_ENTITY_TYPE);
    p.extend_from_slice(&x.to_be_bytes());
    p.extend_from_slice(&y.to_be_bytes());
    p.extend_from_slice(&z.to_be_bytes());
    spawn_angles(&mut p, yaw, pitch);
    write_varint(&mut p, 0);
    p.extend_from_slice(&[0u8; 6]); // velocity
    p
}
#[allow(dead_code)]
fn arrow_meta(eid: i32) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, eid as i64);
    p.push(0xff);
    p
}
fn entity_teleport(eid: i32, x: f64, y: f64, z: f64, yaw: f32, pitch: f32, g: bool) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, eid as i64);
    p.extend_from_slice(&x.to_be_bytes());
    p.extend_from_slice(&y.to_be_bytes());
    p.extend_from_slice(&z.to_be_bytes());
    p.push(teleport_angle(yaw) as u8);
    p.push(teleport_angle(pitch) as u8);
    p.push(g as u8);
    p
}
fn head_rot(eid: i32, yaw: f32) -> Vec<u8> {
    connection_limits::entity_head_rotation(eid, angle(yaw) as u8)
}
// LOCK ORDER: acquires clients() internally — the caller must NOT
// hold clients().lock() (self-deadlock). Per-client field locks are
// fine: the client list is snapshotted before any field is touched.
/// Snapshot the client list and release the lock before handing it back. Every
/// caller must come through here, because sending while holding `clients()`
/// self-deadlocks: the send paths acquire it themselves. That was hit twice
/// before the rule was written down, so the rule is a function rather than a
/// comment that the next caller has to find.
fn client_snapshot() -> Vec<Arc<Client>> {
    clients().lock().unwrap().iter().cloned().collect()
}

/// Send one packet to every client the predicate accepts. The snapshot is taken
/// before any send, so the lock order above is structural rather than remembered.
/// The audience is a parameter, not a global read: the four broadcast helpers
/// are pinned by a test that hands them its own three clients, and that test
/// asserts that the *other* clients receive nothing. While the audience came
/// from `clients()`, any test running beside it that broadcast to the global
/// list wrote into those sockets and broke the negative assertions - measured
/// once in four eight-thread runs (`fanout_tests`, 68 bytes on the sender).
fn fanout_in<F, P>(audience: &[Arc<Client>], interested: P, mk: F)
where
    F: Fn(&Client) -> (i64, Vec<u8>),
    P: Fn(&Client) -> bool,
{
    let mut shared = None;
    fanout_shared_in(audience, interested, mk, &mut shared);
}

/// `fanout_in` with the broadcast's frame cache handed in rather than owned:
/// production callers pass a fresh `None`; a test keeps the cache afterwards
/// and reads `frame.encode_count()`, the direct proof that a fanout encoded
/// once for every identical packet instead of once per recipient.
fn fanout_shared_in<F, P>(
    audience: &[Arc<Client>],
    interested: P,
    mk: F,
    shared: &mut Option<BroadcastFrameCache>,
) where
    F: Fn(&Client) -> (i64, Vec<u8>),
    P: Fn(&Client) -> bool,
{
    for c in audience.iter() {
        if !interested(c) {
            continue;
        }
        let (id, packet) = mk(c);
        if let Ok(mut w) = c.sock.lock() {
            let _ = send_broadcast_frame(&mut w, shared, id, &packet);
        }
    }
}

fn broadcast<F: Fn(&Client) -> (i64, Vec<u8>)>(me_eid: i32, mk: F) {
    broadcast_in(&client_snapshot(), me_eid, mk)
}

/// The next container stateId for windows tracked by `inv_state_id`, wrapping
/// like vanilla `AbstractContainerMenu.incrementStateId` (`& 0x7fff`).
fn inv_state_next(me: &Client) -> i32 {
    (me.inv_state_id.fetch_add(1, Ordering::Relaxed) + 1) & 0x7fff
}

/// Whether the stateId the client echoed on `0x0e` no longer matches the
/// authoritative counter for `wid`. Unknown windows are not validated.
fn container_state_stale(me: &Client, wid: u8, click_state_id: i32) -> bool {
    let expected = match wid {
        0 | 2 | 3 => me.inv_state_id.load(Ordering::Relaxed),
        1 => me
            .chest_sync
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0, |sync| sync.state_id),
        _ => return false,
    };
    click_state_id != expected
}

/// Full `0x13` SetContainerContent resync after a stale stateId click: the
/// authoritative counter advances and every slot is resent, so the client and
/// server re-agree instead of drifting apart.
fn resend_container_content(me: &Client, wid: u8) {
    let inv = me.inv.lock().unwrap().clone();
    let cursor = me.cursor.lock().unwrap().clone();
    let empty = Stack::default();
    let mut q = vec![wid];
    match wid {
        0 => {
            write_varint(&mut q, i64::from(inv_state_next(me)));
            write_varint(&mut q, 46);
            for slot in 0..46i16 {
                q.extend_from_slice(&slot_wire(inv.get(&slot).unwrap_or(&empty)));
            }
        }
        1 => {
            let Some(window) = *me.open_chest.lock().unwrap() else {
                return;
            };
            let slots = chest_window_slots(window);
            let state_id = {
                let mut sync = me.chest_sync.lock().unwrap();
                let sync = sync.get_or_insert_with(|| ChestSyncState {
                    window,
                    anchor: None,
                    state_id: 0,
                    last_slots: slots.clone(),
                });
                sync.state_id = (sync.state_id + 1) & 0x7fff;
                sync.last_slots = slots.clone();
                sync.state_id
            };
            write_varint(&mut q, i64::from(state_id));
            write_varint(&mut q, (slots.len() + 36) as i64);
            for stack in &slots {
                q.extend_from_slice(&slot_wire(stack));
            }
            for slot in 9..45i16 {
                q.extend_from_slice(&slot_wire(inv.get(&slot).unwrap_or(&empty)));
            }
        }
        2 => {
            let Some((dim, x, y, z)) = *me.open_furnace.lock().unwrap() else {
                return;
            };
            write_varint(&mut q, i64::from(inv_state_next(me)));
            write_varint(&mut q, 39);
            for stack in furnace_slot_snapshot_dim(dim, (x, y, z)) {
                q.extend_from_slice(&slot_wire(&stack));
            }
            for slot in 3..39i16 {
                q.extend_from_slice(&slot_wire(inv.get(&(slot + 6)).unwrap_or(&empty)));
            }
        }
        3 => {
            let craft9 = me.craft9.lock().unwrap().clone();
            write_varint(&mut q, i64::from(inv_state_next(me)));
            write_varint(&mut q, 46);
            let result = json_craft_match(&craft9, 3).unwrap_or_default();
            q.extend_from_slice(&slot_wire(&result));
            for slot in 1..=9i16 {
                q.extend_from_slice(&slot_wire(craft9.get(&slot).unwrap_or(&empty)));
            }
            for slot in 10..46i16 {
                q.extend_from_slice(&slot_wire(inv.get(&(slot - 1)).unwrap_or(&empty)));
            }
        }
        _ => return,
    }
    q.extend_from_slice(&slot_wire(&cursor));
    if let Ok(mut w) = me.sock.lock() {
        let _ = send_packet(&mut w, 0x13, q);
    }
}

/// Entity metadata for pose 6 (`SLEEPING`) + LivingEntity's optional sleeping
/// bed position at index 14. Vanilla broadcasts the full entity data on pose
/// changes; for parity of the sleep animation these two entries suffice.
fn sleep_meta_pkt(eid: i32, sleeping: bool, bed: (i32, i32, i32)) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, i64::from(eid));
    p.push(6);
    p.push(21); // POSE
    write_varint(&mut p, if sleeping { 2 } else { 0 });
    p.push(14); // SLEEPING_POS_ID
    p.push(27); // OPTIONAL_POSITION
    p.push(u8::from(sleeping));
    if sleeping {
        write_pos(&mut p, bed.0, bed.1, bed.2);
    }
    p.push(0xff);
    p
}

fn broadcast_sleep_pose(me: &Client, sleeping: bool, bed: (i32, i32, i32)) {
    let pkt = sleep_meta_pkt(me.eid, sleeping, bed);
    broadcast_seen(me.eid, |_c| (0x58, pkt.clone()));
}

/// Wake the player if they are in a bed: clear the state and restore the pose
/// for observers. Returns true when the player was sleeping.
fn wake_if_sleeping(me: &Client) -> bool {
    if let Some(state) = me.sleeping.lock().unwrap().take() {
        broadcast_sleep_pose(me, false, state.bed);
        true
    } else {
        false
    }
}

/// Vanilla `ServerLevel.wakeUpAllPlayers` + `advanceWeatherCycle` on the night
/// skip: jump to the next morning, clear rain/thunder and wake every sleeper.
fn skip_night() {
    let day = WORLD_TIME.load(Ordering::Relaxed).div_euclid(24000);
    let morning = (day + 1) * 24000;
    WORLD_TIME.store(morning, Ordering::Relaxed);
    // `RAINING`/`THUNDERING` count the remaining ticks of weather.
    RAINING.store(0, Ordering::Relaxed);
    THUNDERING.store(0, Ordering::Relaxed);
    let sleepers: Vec<Arc<Client>> = client_snapshot()
        .into_iter()
        .filter(|c| c.sleeping.lock().unwrap().is_some())
        .collect();
    let mut tm = Vec::new();
    tm.extend_from_slice(&0i64.to_be_bytes());
    tm.extend_from_slice(&morning.to_be_bytes());
    let mut rain = vec![2u8]; // END_RAINING
    rain.extend_from_slice(&0f32.to_be_bytes());
    for sleeper in sleepers {
        let bed = sleeper.sleeping.lock().unwrap().take().map_or((0, 0, 0), |s| s.bed);
        broadcast_sleep_pose(&sleeper, false, bed);
        if let Ok(mut w) = sleeper.sock.lock() {
            let _ = send_packet(&mut w, 0x64, tm.clone());
            let _ = send_packet(&mut w, 0x22, rain.clone());
        }
        let line = system_chat(&format!("{} slept through the night", sleeper.name));
        broadcast(-1, |_c| (0x6c, line.clone()));
    }
}

/// One server tick of sleep bookkeeping: wake on fresh damage, age every
/// sleeper, then run vanilla's
/// `sleepingPlayers * 100 >= playersSleepingPercentage * allPlayers` gate once
/// at least one player has slept for 100 ticks.
fn sleep_tick() {
    let clients = client_snapshot();
    let mut any_ready = false;
    for c in &clients {
        let mut sl = c.sleeping.lock().unwrap();
        let Some(mut state) = *sl else {
            continue;
        };
        let last_hurt = *c.last_hurt.lock().unwrap();
        if last_hurt > state.last_hurt {
            let bed = state.bed;
            *sl = None;
            drop(sl);
            broadcast_sleep_pose(c, false, bed);
            continue;
        }
        state.ticks = state.ticks.saturating_add(1);
        if state.ticks >= 100 {
            any_ready = true;
        }
        *sl = Some(state);
    }
    if !any_ready {
        return;
    }
    // Vanilla counts everyone in the Overworld (awake or asleep) in the
    // denominator; `playersSleepingPercentage` > 100 can never be met.
    let (sleeping, all) = clients
        .iter()
        .filter(|c| *c.dim.lock().unwrap() == DimId::Overworld)
        .fold((0i64, 0i64), |(s, a), c| {
            (s + i64::from(c.sleeping.lock().unwrap().is_some()), a + 1)
        });
    if all == 0 {
        return;
    }
    let percentage = command_gamerules()
        .lock()
        .unwrap()
        .get("playersSleepingPercentage")
        .copied()
        .unwrap_or(100);
    // `0` means a single sleeper can skip the night once they are done lying
    // down; `> 100` disables sleep-skipping entirely.
    if sleeping > 0 && sleeping * 100 >= i64::from(percentage) * all {
        skip_night();
    }
}

/// Bed use at night: vanilla `startSleepInBed` — mark the player sleeping
/// (respawn was already set by the caller) and broadcast the pose change so
/// observers see the lying-down animation. Actual time travel happens in
/// `sleep_tick` after 100 slept ticks.
fn enter_bed(me: &Client, x: i32, y: i32, z: i32) {
    *me.sleeping.lock().unwrap() = Some(Sleeping {
        bed: (x, y, z),
        ticks: 0,
        last_hurt: *me.last_hurt.lock().unwrap(),
    });
    broadcast_sleep_pose(me, true, (x, y, z));
}
fn broadcast_in<F: Fn(&Client) -> (i64, Vec<u8>)>(audience: &[Arc<Client>], me_eid: i32, mk: F) {
    fanout_in(audience, |c| c.eid != me_eid, mk)
}
struct TrackedSpawn {
    dim: DimId,
    range: u32,
    job: SpawnJob,
}

fn entity_tracking_range(etype: i64) -> u32 {
    static RANGES: OnceLock<Vec<u32>> = OnceLock::new();
    let ranges = RANGES.get_or_init(|| {
        ENTITY_REGISTRY_1211
            .iter()
            .map(|spec| client_tracking_range_chunks(spec.name))
            .collect()
    });
    usize::try_from(etype)
        .ok()
        .and_then(|id| ranges.get(id))
        .copied()
        .unwrap_or(0)
}

fn entity_in_tracking_range(player: (f64, f64), target: (f64, f64), range: u32, view: i32) -> bool {
    let distance = f64::from(range.min(view as u32)) * 16.0;
    let dx = player.0 - target.0;
    let dz = player.1 - target.1;
    range > 0 && dx * dx + dz * dz <= distance * distance
}

fn interest_tick() {
    let cls = client_snapshot();
    if cls.is_empty() {
        return;
    }
    sleep_tick();
    // Build spawn payloads only when an observer starts tracking the entity.
    let mut ents = Vec::new();
    let mut queue = |dim, etype, job| {
        ents.push(TrackedSpawn {
            dim,
            range: entity_tracking_range(etype),
            job,
        });
    };
    for cart in generated_content().lock().unwrap().carts.iter() {
        let cart = cart.clone();
        let [x, y, z] = cart.pos().expect("validated generated cart");
        queue(
            DimId::Overworld,
            name_to_etype("chest_minecart").unwrap(),
            (
                cart.eid,
                x,
                y,
                z,
                Box::new(move || vec![(0x01, spawn_generated_cart(&cart))]),
            ),
        );
    }
    for frame in generated_content().lock().unwrap().frames.iter() {
        let frame = frame.clone();
        let [x, y, z] = frame.pos().expect("validated generated frame");
        queue(
            DimId::Overworld,
            name_to_etype(if frame.glow() {
                "glow_item_frame"
            } else {
                "item_frame"
            })
            .unwrap(),
            (
                frame.eid,
                x,
                y,
                z,
                Box::new(move || {
                    vec![
                        (0x01, spawn_generated_frame(&frame)),
                        (0x58, frame_meta_pkt(&frame)),
                    ]
                }),
            ),
        );
    }
    let mut live_eids = HashSet::new();
    for m in mobs().lock().unwrap().iter() {
        let p = *m.pos.lock().unwrap();
        let m = m.clone();
        live_eids.insert(m.eid);
        queue(
            m.dim,
            m.etype,
            (
                m.eid,
                p.0,
                p.1,
                p.2,
                Box::new(move || {
                    let mut packets = live_mob_spawn_packets(&m);
                    // SetEquipment trails the spawn so a skeleton's bow or a
                    // pillager's crossbow is visible from the first frame.
                    if let Some(equipment) = mob_equipment_packet(&m) {
                        packets.push((0x5b, equipment));
                    }
                    packets
                }),
            ),
        );
    }
    mob_equipment()
        .lock()
        .unwrap()
        .retain(|eid, _| live_eids.contains(eid));
    for &projectile in live_mob_projectiles().lock().unwrap().iter() {
        let [x, y, z] = projectile.pos;
        queue(
            projectile.dim,
            projectile.entity_type(),
            (
                projectile.eid,
                x,
                y,
                z,
                Box::new(move || live_mob_projectile_packets(projectile)),
            ),
        );
    }
    for cloud in live_dragon_clouds().lock().unwrap().iter() {
        let (eid, [x, y, z]) = (cloud.eid, cloud.pos);
        queue(
            cloud.dim,
            1,
            (
                eid,
                x,
                y,
                z,
                Box::new(move || vec![(0x01, spawn_typed_pkt(eid, 1, x, y, z))]),
            ),
        );
    }
    for d in drops().lock().unwrap().iter() {
        let (eid, x, y, z, item, cnt, comps) =
            (d.eid, d.x, d.y, d.z, d.item, d.cnt, d.comps.clone());
        queue(
            DimId::Overworld,
            name_to_etype("item").unwrap(),
            (
                eid,
                x,
                y,
                z,
                Box::new(move || {
                    let d = Drop {
                        eid,
                        x,
                        y,
                        z,
                        item,
                        cnt,
                        comps: comps.clone(),
                        age: 0,
                        target: None,
                        thrower: None,
                    };
                    vec![(0x01, spawn_drop_pkt(&d)), (0x58, drop_meta_pkt(&d))]
                }),
            ),
        );
    }
    for a in arrows().lock().unwrap().iter() {
        let a2 = Arrow {
            eid: a.eid,
            owner: a.owner,
            x: a.x,
            y: a.y,
            z: a.z,
            dx: a.dx,
            dy: a.dy,
            dz: a.dz,
            ttl: a.ttl,
            dmg: a.dmg,
            fire: a.fire,
            punch: a.punch,
            kind: 0,
            loy: 0,
            ret_item: 0,
            ret_comps: Vec::new(),
            pierce: 0,
            last_hit: -1,
            stuck: false,
        };
        queue(
            DimId::Overworld,
            4,
            (
                a.eid,
                a.x,
                a.y,
                a.z,
                Box::new(move || vec![(0x01, spawn_arrow_pkt(&a2))]),
            ),
        );
    }
    for b in bobbers().lock().unwrap().iter() {
        let (eid, x, y, z) = (b.eid, b.x, b.y, b.z);
        queue(
            DimId::Overworld,
            129,
            (
                eid,
                x,
                y,
                z,
                Box::new(move || vec![(0x01, spawn_typed_pkt(eid, 129, x, y, z))]),
            ),
        );
    }
    for p in pearls().lock().unwrap().iter() {
        let (eid, x, y, z) = (p.eid, p.x, p.y, p.z);
        queue(
            DimId::Overworld,
            32,
            (
                eid,
                x,
                y,
                z,
                Box::new(move || vec![(0x01, spawn_typed_pkt(eid, 32, x, y, z))]),
            ),
        );
    }
    for sb in snowballs().lock().unwrap().iter() {
        let (eid, x, y, z) = (sb.eid, sb.x, sb.y, sb.z);
        queue(
            DimId::Overworld,
            97,
            (
                eid,
                x,
                y,
                z,
                Box::new(move || vec![(0x01, spawn_typed_pkt(eid, 97, x, y, z))]),
            ),
        );
    }
    for t in primed().lock().unwrap().iter() {
        let (eid, x, y, z) = (t.eid, t.x, t.y, t.z);
        queue(
            DimId::Overworld,
            106,
            (
                eid,
                x,
                y,
                z,
                Box::new(move || vec![(0x01, spawn_entity_pkt(eid, 106, x, y, z, 0.0, 0.0, 0))]),
            ),
        );
    }
    for f in falling().lock().unwrap().iter() {
        let (eid, x, y, z, state) = (f.eid, f.x, f.y, f.z, f.state);
        queue(
            DimId::Overworld,
            name_to_etype("falling_block").unwrap(),
            (
                eid,
                x,
                y,
                z,
                Box::new(move || vec![(0x01, spawn_falling_pkt(eid, state, x, y, z))]),
            ),
        );
    }
    for o in xps().lock().unwrap().iter() {
        let (eid, x, y, z, val) = (o.eid, o.x, o.y, o.z, o.val);
        queue(
            DimId::Overworld,
            name_to_etype("experience_orb").unwrap(),
            (
                eid,
                x,
                y,
                z,
                Box::new(move || vec![(0x02, xp_orb_pkt(eid, x, y, z, val))]),
            ),
        );
    }
    // P2-3-S4: a player is only interested in players sharing their dimension. Two players
    // at the same coordinates in different worlds must not spawn for one another; on a
    // dimension switch the 0x42 removals below retire the old-world players.
    for o in cls.iter() {
        let (dim, x, y, z, _, _) = client_dim_pos(o);
        let o2 = o.clone();
        queue(
            dim,
            PLAYER_ENTITY_TYPE,
            (
                o.eid,
                x,
                y,
                z,
                Box::new(move || {
                    let mut packets = vec![(0x01, spawn_player(&o2))];
                    // SetEquipment trails the spawn so an observer sees the
                    // player's held item and armor from the first frame.
                    if let Some(equipment) = player_equipment_packet(&o2) {
                        packets.push((0x5b, equipment));
                    }
                    packets
                }),
            ),
        );
    }
    for client in &cls {
        sync_entity_interest(client, &ents);
    }
    dig_progress_tick(&cls);
    // Death is a server-tick event (vanilla LivingEntity.die): an idle
    // victim's own read loop may not wake for seconds, so the tick sweep
    // drives drop_player_items_on_death — it emits the 0x1f death event and
    // drops exactly once via death_drop_marks.
    for c in &cls {
        drop_player_items_on_death(c);
    }
}

/// Server-side mining completion: a pending START_DESTROY_BLOCK whose block
/// still matches and whose dig duration has elapsed breaks, exactly as
/// `ServerPlayerGameMode.tick` advancing `destroyProgress` past 1.0 would. A
/// changed or removed block, a cancelled dig, or a client disconnect clears the
/// pending state instead.
fn dig_progress_tick(cls: &[Arc<Client>]) {
    for c in cls {
        let Some(dig) = *c.dig.lock().unwrap() else {
            continue;
        };
        let (dim, x, y, z) = dig.pos;
        let current = block_at_dim(dim, x, y, z);
        if current != dig.state {
            *c.dig.lock().unwrap() = None;
            continue;
        }
        if dig.started.elapsed().as_secs_f32() >= dig.seconds {
            *c.dig.lock().unwrap() = None;
            finish_block_break(c, dim, x, y, z, dig.state);
        }
    }
}

/// Step a ray from the eye along the look vector and return the first cell
/// that is not air — the crosshair block `pick_item` resolves. Vanilla's
/// `ProjectileUtil.getHitResultOnViewVector` is a block-grid traversal; a
/// 0.1-length walk reaches the same cells inside the ~5-block interaction
/// range without a hit-box model.
fn ray_pick_block(
    dim: DimId,
    ex: f64,
    ey: f64,
    ez: f64,
    yaw: f32,
    pitch: f32,
    reach: f64,
) -> Option<(i32, i32, i32)> {
    let yaw = f64::from(yaw).to_radians();
    let pitch = f64::from(pitch).to_radians();
    let (dx, dy, dz) = (-yaw.sin() * pitch.cos(), -pitch.sin(), yaw.cos() * pitch.cos());
    let (mut x, mut y, mut z) = (ex, ey, ez);
    let mut last = (i32::MIN, i32::MIN, i32::MIN);
    let mut walked = 0.0;
    while walked <= reach {
        let cell = (x.floor() as i32, y.floor() as i32, z.floor() as i32);
        if cell != last {
            last = cell;
            if block_at_dim(dim, cell.0, cell.1, cell.2) != 0 {
                return Some(cell);
            }
        }
        x += dx * 0.1;
        y += dy * 0.1;
        z += dz * 0.1;
        walked += 0.1;
    }
    None
}

/// `Inventory.pick`'s decision over menu slot numbers, split out so the
/// packet handler only moves stacks. The match compares item id and empty
/// components, mirroring `isSameItemSameComponents` against the plain block
/// stack the server resolved.
#[derive(Debug, PartialEq, Eq)]
enum PickItemPlan {
    /// The match already sits on the hotbar: select that slot.
    Select(i16),
    /// A main-inventory slot swaps contents with the selected hotbar slot.
    Swap(i16),
    /// Creative invents a copy of the picked stack in the selected slot.
    FillCreative,
}

fn pick_item_plan(
    inv: &HashMap<i16, Stack>,
    item: i64,
    creative: bool,
) -> Option<PickItemPlan> {
    let plain = |st: &Stack| st.0 == item && st.1 > 0 && st.2.is_empty();
    for slot in 36..=44 {
        if inv.get(&slot).is_some_and(plain) {
            return Some(PickItemPlan::Select(slot));
        }
    }
    for slot in 9..=35 {
        if inv.get(&slot).is_some_and(plain) {
            return Some(PickItemPlan::Swap(slot));
        }
    }
    creative.then_some(PickItemPlan::FillCreative)
}

/// `ServerGamePacketListenerImpl.handlePickItem`: ray-trace the looked-at
/// block, `Inventory.pick` its stack into the selection, then resync the
/// touched slots and the carried-item index the way vanilla's packet does.
fn pick_item(me: &Arc<Client>, held_slot: &mut i16) {
    let (px, py, pz, yaw, pitch, _) = *me.pos.lock().unwrap();
    let creative = *me.gm.lock().unwrap() == 1;
    let reach = if creative { 5.6f64 } else { 4.9 };
    let dim = *me.dim.lock().unwrap();
    let Some((bx, by, bz)) = ray_pick_block(dim, px, py + 1.62, pz, yaw, pitch, reach)
    else {
        return;
    };
    let Some(reg) = block_registry::breg() else {
        return;
    };
    let name = reg.name_of(block_at_dim(dim, bx, by, bz));
    let Some(item) = name_to_item(name.trim_start_matches("minecraft:")) else {
        return;
    };
    let selected = 36 + *held_slot;
    let plan = {
        let inv = me.inv.lock().unwrap();
        pick_item_plan(&inv, item, creative)
    };
    match plan {
        Some(PickItemPlan::Select(slot)) => {
            *held_slot = slot - 36;
            *me.held.lock().unwrap() = *held_slot;
        }
        Some(PickItemPlan::Swap(slot)) => {
            let mut inv = me.inv.lock().unwrap();
            let held_stack = inv.get(&selected).cloned().unwrap_or((0, 0, Vec::new()));
            let match_stack = inv.get(&slot).cloned().unwrap_or((0, 0, Vec::new()));
            inv.insert(selected, match_stack);
            if held_stack.1 > 0 {
                inv.insert(slot, held_stack);
            } else {
                inv.remove(&slot);
            }
            drop(inv);
            let stacks: Vec<(i16, Stack)> = [selected, slot]
                .into_iter()
                .map(|wslot| {
                    (
                        wslot,
                        me.inv
                            .lock()
                            .unwrap()
                            .get(&wslot)
                            .cloned()
                            .unwrap_or((0, 0, Vec::new())),
                    )
                })
                .collect();
            for (wslot, st) in stacks {
                resync_slot(me, 0, wslot, &st);
            }
        }
        Some(PickItemPlan::FillCreative) => {
            me.inv.lock().unwrap().insert(selected, (item, 1, Vec::new()));
            held_slot_resync(me);
        }
        None => return,
    }
    // Vanilla closes `handlePickItem` with ClientboundSetHeldSlotPacket.
    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x53, vec![*held_slot as u8]);
}

/// `can_place_on`/`can_break` component ids (protocol 767 data-component
/// registry indices 10 and 11). Both encode `AdventureModePredicate`: a
/// VarInt count of `BlockPredicate` entries — optional block holder-set,
/// optional property-matcher list, optional NBT — where any one matching
/// predicate allows the interaction.
const COMP_CAN_PLACE_ON: i64 = 10;
const COMP_CAN_BREAK: i64 = 11;

/// A `PropertyMatcher`'s min/max arm: `true` when `value` falls inside the
/// closed range. Ordered property values compare numerically when every side
/// parses (vanilla's ordered properties are index-ordered and overwhelmingly
/// numeric); anything else takes the lexicographic answer. Only used where a
/// missing matcher already failed the predicate.
fn prop_value_in_range(value: &str, min: &str, max: &str) -> bool {
    match (value.parse::<i64>(), min.parse::<i64>(), max.parse::<i64>()) {
        (Ok(v), Ok(lo), Ok(hi)) => v >= lo && v <= hi,
        _ => value >= min && value <= max,
    }
}

/// Parse one `BlockPredicate` off `b` and test the parts a block state can
/// answer: the holder-set (explicit ids or a named tag) and the property
/// matchers. Returns `Ok(false)` for a parsed-but-unmatched predicate and
/// `Err(())` for truncated/unsupported input — including a present NBT clause,
/// which needs a block entity this layer does not see and therefore fails the
/// whole component closed.
fn adventure_predicate_match(
    reg: &block_registry::BReg,
    st: u32,
    b: &mut &[u8],
) -> Result<bool, ()> {
    // Optional<HolderSet<Block>>: absent matches every block. On the wire a
    // negative size (-1) names a registry tag; otherwise `n` VarInt block ids.
    let has_blocks = *b.first().ok_or(())? != 0;
    *b = &b[1..];
    if has_blocks {
        let n = read_varint(b).ok_or(())?;
        let in_set = if n < 0 {
            let tag = read_string_bounded(b, 32767).ok_or(())?;
            reg.in_tag(st, &tag)
        } else {
            if n > 4096 {
                return Err(());
            }
            let mut hit = false;
            for _ in 0..n {
                hit |= reg.block_id_of(st) == Some(read_varint(b).ok_or(())? as i32);
            }
            hit
        };
        if !in_set {
            return Ok(false);
        }
    }
    // Optional<List<PropertyMatcher>>: absent matches every state.
    let has_props = *b.first().ok_or(())? != 0;
    *b = &b[1..];
    if has_props {
        let count = read_varint(b).ok_or(())?;
        if !(0..=256).contains(&count) {
            return Err(());
        }
        for _ in 0..count {
            let name = read_string_bounded(b, 32767).ok_or(())?;
            let kind = *b.first().ok_or(())?;
            *b = &b[1..];
            let value = reg.get_prop(st, &name);
            let ok = match kind {
                0 => {
                    let want = read_string_bounded(b, 32767).ok_or(())?;
                    value.as_deref() == Some(want.as_str())
                }
                1 => {
                    let min = read_string_bounded(b, 32767).ok_or(())?;
                    let max = read_string_bounded(b, 32767).ok_or(())?;
                    value.is_some_and(|v| prop_value_in_range(&v, &min, &max))
                }
                _ => return Err(()),
            };
            if !ok {
                return Ok(false);
            }
        }
    }
    // Optional<Nbt>: a present clause needs the block entity and cannot be
    // skipped byte-wise here, so the component fails closed.
    if *b.first().ok_or(())? != 0 {
        return Err(());
    }
    *b = &b[1..];
    Ok(true)
}

/// `AdventureModePredicate.test` over a held stack's `can_place_on` /
/// `can_break` component against one block state. A stack without the
/// component allows nothing in adventure mode; a malformed or
/// NBT-bearing component is treated the same — deny.
fn adventure_stack_allows(reg: &block_registry::BReg, st: u32, held: &Stack, comp: i64) -> bool {
    let Some(data) = comp_get(&held.2, comp) else {
        return false;
    };
    let mut b = data;
    let Some(count) = read_varint(&mut b) else {
        return false;
    };
    if !(0..=256).contains(&count) {
        return false;
    }
    for _ in 0..count {
        match adventure_predicate_match(reg, st, &mut b) {
            Ok(true) => return true,
            Ok(false) => {}
            Err(()) => return false,
        }
    }
    false
}

/// `TrapDoorBlock.useWithoutItem`: every trapdoor but the iron one toggles
/// `open` on right-click; the iron variant answers redstone only. A toggled
/// state is pushed to every chunk watcher through `note_block_update_dim`.
/// Returns whether a toggle ran.
fn try_toggle_trapdoor(dim: DimId, x: i32, y: i32, z: i32) -> bool {
    let st = block_at_dim(dim, x, y, z);
    if !trapdoor_state(st) {
        return false;
    }
    let Some(reg) = block_registry::breg() else {
        return false;
    };
    if reg.name_of(st) == "minecraft:iron_trapdoor" {
        return false;
    }
    let Some(opened) = reg.get_prop(st, "open").map(|v| v == "true") else {
        return false;
    };
    let updated = reg.with_prop(st, "open", if opened { "false" } else { "true" });
    set_block_dim(dim, x, y, z, updated);
    note_block_update_dim(dim, x, y, z, updated);
    true
}

/// Apply a confirmed block break: clear the cell, fan the update out to every
/// chunk watcher, then run the survival bookkeeping (exhaustion, tool wear,
/// drops). Shared by the dig packet's immediate path and the tick-driven
/// destroy progress that runs after START_DESTROY_BLOCK.
fn finish_block_break(me: &Arc<Client>, dim: DimId, x: i32, y: i32, z: i32, old: u32) {
    let held = {
        let held_slot = *me.held.lock().unwrap();
        let inv = me.inv.lock().unwrap();
        inv.get(&(36 + held_slot)).cloned().unwrap_or((0, 0, Vec::new()))
    };
    // Vanilla rechecks mayDestroy in `destroyAndAck`: an adventure-mode dig
    // still finishes only while the held stack's can_break matches.
    if *me.gm.lock().unwrap() == 2 {
        let allowed = block_registry::breg().is_some_and(|reg| {
            adventure_stack_allows(reg, old, &held, COMP_CAN_BREAK)
        });
        if !allowed {
            return;
        }
    }
    // Live progression gates (mod-suite): a sealed dimension/biome rule
    // denies the break before any world mutation and resyncs the cell so the
    // client sees the block reappear, matching the recheck above.
    if live_progression_denies_break(me, dim, x, y, z) {
        return;
    }
    #[cfg(feature = "neoforge-events")]
    if java_compat::NEOFORGE.get().is_some() {
        let result = neoforge_event(me, java_compat::S1Event::BlockBreak {
            dim: dim.index() as u8,
            pos: (x, y, z),
            state: old,
            player: (me.eid, me.name.clone()),
            exp_to_drop: 0,
        });
        if result.is_err() || !set_block_if_dim(dim, x, y, z, 0, Some(old)) {
            neoforge_resync_block(me, dim, (x, y, z));
            return;
        }
    } else {
        set_block_dim(dim, x, y, z, 0);
    }
    #[cfg(not(feature = "neoforge-events"))]
    set_block_dim(dim, x, y, z, 0);
    note_block_update_dim(dim, x, y, z, 0);
    if *me.gm.lock().unwrap() != 1 {
        *me.walk_acc.lock().unwrap() += 0.005; // block-break exhaustion
        // `Stats.BLOCK_MINED` for the block that was there — keyed by the
        // block item registry id like every other mined stat entry.
        if let Some(reg) = crate::block_registry::breg() {
            if let Some(block_id) = reg.block_id_of(old) {
                stat_increment(me, StatCategory::Mined, block_id, 1);
            }
        }
                                               // vanilla: breaking a block damages the held tool
        if tool_max_damage(held.0).is_some()
            || comp_get(&held.2, COMP_MAX_DAMAGE).is_some()
        {
            if damage_held(me) {
                let (pxx, pyy, pzz, ..) = {
                    let p = me.pos.lock().unwrap();
                    *p
                };
                let snd = sound_pkt_named("entity.item.break", pxx, pyy + 1.0, pzz);
                broadcast_near(
                    *me.dim.lock().unwrap(),
                    pxx,
                    pyy + 1.0,
                    pzz,
                    -1,
                    |_c| (0x68, snd.clone()),
                );
            }
            held_slot_resync(me);
        }
        let silk = enchant_level(&held.2, 33) > 0;
        let fort = enchant_level(&held.2, 13).clamp(0, 3) as u8;
        // ore loot: (material drop, silk-touch drops the ore block
        // item instead, fortune multiplies the material count)
        let ore: Option<(i64, i64)> = match old {
            127 | 128 => Some((803, 62)),     // coal -> coal_ore
            125 | 126 => Some((810, 64)),     // raw_iron -> iron_ore
            123 | 124 => Some((814, 68)),     // raw_gold -> gold_ore
            4274 | 4275 => Some((805, 76)),   // diamond -> diamond_ore
            520 | 521 => Some((807, 74)),     // lapis -> lapis_ore
            5735..=5737 => Some((657, 70)),   // redstone -> redstone_ore
            22942..=22943 => Some((812, 66)), // raw_copper -> copper_ore
            7511 | 7512 => Some((806, 72)),   // emerald -> emerald_ore
            _ => None,
        };
        let (drop_item, drop_cnt) = if let (true, Some(o)) = (silk, ore) {
            (Some(o.1), 1)
        } else {
            let it = match old {
                127 | 128 => Some(803),
                125 | 126 => Some(810),
                123 | 124 => Some(814),
                4274 | 4275 => Some(805),
                520 | 521 => Some(807),
                5735..=5737 => Some(657),
                22942..=22943 => Some(812),
                7511 | 7512 => Some(806),
                4278..=4285 => Some(853), // wheat -> seeds
                _ => block_drop(old),
            };
            let n = if fort > 0 && ore.is_some() {
                1 + (world_rng::next_u64() % (fort as u64 + 1)) as u8
            } else {
                1
            };
            (it, n)
        };
        crops().lock().unwrap().remove(&(dim, x, y, z));
        if old == 4285 && GR_DO_TILE_DROPS.load(Ordering::Relaxed) != 0 {
            // ripe: extra wheat drop
            let d = Drop {
                eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                x: x as f64 + 0.5,
                y: y as f64 + 0.25,
                z: z as f64 + 0.5,
                item: 854,
                cnt: 1,
                comps: Vec::new(),
                age: 0,
                target: None,
                thrower: None,
            };
            drops().lock().unwrap().push(d);
        }
        if let Some(item) = if !can_harvest(old as i64, held.0)
            || GR_DO_TILE_DROPS.load(Ordering::Relaxed) == 0
        {
            None
        } else {
            drop_item
        } {
            let d = Drop {
                eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                x: x as f64 + 0.5,
                y: y as f64 + 0.25,
                z: z as f64 + 0.5,
                item,
                cnt: drop_cnt,
                comps: Vec::new(),
                age: 0,
                target: None,
                thrower: None,
            };
            drops().lock().unwrap().push(d);
        }
    }
}

fn sync_entity_interest(client: &Client, entities: &[TrackedSpawn]) {
    let mut seen = client.seen.lock().unwrap();
    let chunks = client.chunks.lock().unwrap();
    let (dim, px, _, pz, _, _) = client_dim_pos(client);
    let view = *client.view.lock().unwrap();
    let mut eligible = HashMap::new();
    for TrackedSpawn {
        dim: entity_dim,
        range,
        job: (eid, x, _, z, spawn),
    } in entities
    {
        if *eid != client.eid
            && *entity_dim == dim
            && entity_in_tracking_range((px, pz), (*x, *z), *range, view)
            && chunks.contains(&(spatial_chunk_coord(*x), spatial_chunk_coord(*z)))
        {
            eligible.insert(*eid, spawn);
        }
    }
    let previous: Vec<_> = seen.iter().copied().collect();
    let next: Vec<_> = eligible.keys().copied().collect();
    let (added, _, removed) = compute_tracking_diff(&previous, &next);
    if added.is_empty() && removed.is_empty() {
        return;
    }
    let Ok(mut socket) = client.sock.lock() else {
        return;
    };
    if !removed.is_empty() {
        if send_packet(&mut socket, 0x42, remove_entities_payload(&removed)).is_err() {
            return;
        }
        for eid in removed {
            seen.remove(&eid);
        }
    }
    for eid in added {
        for (id, body) in eligible[&eid]() {
            if id == 0x01 {
                with_play_wire(&socket, |play| play.remember_spawn(&body));
            }
            if send_packet(&mut socket, id, body).is_err() {
                return;
            }
        }
        seen.insert(eid);
    }
}

fn remove_entities_payload(entities: &[i32]) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, entities.len() as i64);
    for &eid in entities {
        write_varint(&mut p, i64::from(eid));
    }
    p
}

// entity-scoped broadcast: only clients currently tracking <eid> —
// the tightest possible audience for per-entity update packets.
// chunk-scoped broadcast: only clients currently holding the chunk —
// block updates to an unloaded chunk would create phantom state.
#[cfg(test)]
fn broadcast_chunk_in<F: Fn(&Client) -> (i64, Vec<u8>)>(
    audience: &[Arc<Client>],
    cx: i32,
    cz: i32,
    mk: F,
) {
    broadcast_chunk_dim_in(DimId::Overworld, audience, cx, cz, mk)
}
/// The dimension-scoped chunk audience: a client only receives a block delta if it is in the
/// same dimension AND holds that column. Two players standing at the same `(cx, cz)` in
/// different dimensions must never see each other's edits.
fn broadcast_chunk_dim<F: Fn(&Client) -> (i64, Vec<u8>)>(dim: DimId, cx: i32, cz: i32, mk: F) {
    broadcast_chunk_dim_in(dim, &client_snapshot(), cx, cz, mk)
}
fn broadcast_chunk_dim_in<F: Fn(&Client) -> (i64, Vec<u8>)>(
    dim: DimId,
    audience: &[Arc<Client>],
    cx: i32,
    cz: i32,
    mk: F,
) {
    let mut shared = None;
    for client in audience {
        // Stream and unload take the chunk lock before the socket. Keep the
        // audience check in that order so a delta cannot follow an unload.
        let chunks = client.chunks.lock().unwrap();
        if !chunks.contains(&(cx, cz)) {
            continue;
        }
        let Ok(mut socket) = client.sock.lock() else {
            continue;
        };
        if *client.dim.lock().unwrap() != dim {
            continue;
        }
        let (id, packet) = mk(client);
        if send_broadcast_frame(&mut socket, &mut shared, id, &packet).is_err() {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    }
}
fn broadcast_seen_destroy(eid: i32) {
    let p = remove_entities_payload(&[eid]);
    broadcast_seen(eid, |_c| (0x42, p.clone()));
}
// LOCK ORDER: these helpers acquire clients() internally — never call
// while holding clients().lock() (self-deadlock; hit twice: v1.56
// save_world drops(), v1.76 /effect). Collect targets under the lock,
// release, then broadcast.
fn broadcast_seen<F: Fn(&Client) -> (i64, Vec<u8>)>(eid: i32, mk: F) {
    broadcast_seen_ex(eid, 0, mk)
}
fn broadcast_seen_ex<F: Fn(&Client) -> (i64, Vec<u8>)>(eid: i32, excl: i32, mk: F) {
    broadcast_seen_ex_in(&client_snapshot(), eid, excl, mk)
}
fn broadcast_seen_ex_in<F: Fn(&Client) -> (i64, Vec<u8>)>(
    audience: &[Arc<Client>],
    eid: i32,
    excl: i32,
    mk: F,
) {
    let mut shared = None;
    for client in audience.iter().filter(|c| c.eid != excl) {
        // Serialize updates with the tracking sweep's spawn and remove packets.
        let seen = client.seen.lock().unwrap();
        if seen.contains(&eid) {
            let (id, packet) = mk(client);
            if let Ok(mut socket) = client.sock.lock() {
                let _ = send_broadcast_frame(&mut socket, &mut shared, id, &packet);
            }
        }
    }
}

/// Position-scoped broadcast: vanilla `PlayerList.broadcast(except, x, y, z,
/// radius, dimension, packet)` reaches only same-dimension players strictly
/// inside `radius` of the effect. The radius is read from the packet itself
/// (`connection_limits::broadcast_radius`), so a packet id with no vanilla
/// range contract — chat, weather, player-info — is refused rather than sent
/// to the whole list; this is for sounds, particles and explosions only.
fn broadcast_near<F: Fn(&Client) -> (i64, Vec<u8>)>(
    dim: DimId,
    x: f64,
    y: f64,
    z: f64,
    me_eid: i32,
    mk: F,
) {
    broadcast_near_in(dim, &client_snapshot(), x, y, z, me_eid, mk)
}
fn broadcast_near_in<F: Fn(&Client) -> (i64, Vec<u8>)>(
    dim: DimId,
    audience: &[Arc<Client>],
    x: f64,
    y: f64,
    z: f64,
    me_eid: i32,
    mk: F,
) {
    let mut shared = None;
    for client in audience.iter().filter(|c| c.eid != me_eid) {
        if *client.dim.lock().unwrap() != dim {
            continue;
        }
        let (id, packet) = mk(client);
        let Some(radius) = connection_limits::broadcast_radius(id, &packet) else {
            continue;
        };
        let (px, py, pz, ..) = *client.pos.lock().unwrap();
        if !connection_limits::within_broadcast_range(px, py, pz, x, y, z, radius) {
            continue;
        }
        if let Ok(mut socket) = client.sock.lock() {
            let _ = send_broadcast_frame(&mut socket, &mut shared, id, &packet);
        }
    }
}

// broadcast helpers return (packet_id, payload)

fn spawn_arrow_pkt(a: &Arrow) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, a.eid as i64);
    p.extend_from_slice(&[0u8; 16]);
    write_varint(&mut p, 4); // arrow
    p.extend_from_slice(&a.x.to_be_bytes());
    p.extend_from_slice(&a.y.to_be_bytes());
    p.extend_from_slice(&a.z.to_be_bytes());
    p.extend_from_slice(&[0u8; 3]);
    write_varint(&mut p, 0);
    p.extend_from_slice(&[0u8; 6]);
    p
}

fn spawn_typed_pkt(eid: i32, etype: i64, x: f64, y: f64, z: f64) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, eid as i64);
    p.extend_from_slice(&[0u8; 16]);
    write_varint(&mut p, etype);
    p.extend_from_slice(&x.to_be_bytes());
    p.extend_from_slice(&y.to_be_bytes());
    p.extend_from_slice(&z.to_be_bytes());
    p.extend_from_slice(&[0u8; 3]);
    write_varint(&mut p, 0);
    p.extend_from_slice(&[0u8; 6]);
    p
}

fn read_varint_at(b: &[u8], p: usize) -> (i64, usize) {
    let mut r: i64 = 0;
    let mut i = 0;
    loop {
        let byte = b[p + i];
        r |= ((byte & 0x7f) as i64) << (7 * i);
        if byte & 0x80 == 0 {
            return (r, p + i + 1);
        }
        i += 1;
    }
}

fn velocity_pkt(eid: i32, vx: i16, vy: i16, vz: i16) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, eid as i64);
    p.extend_from_slice(&vx.to_be_bytes());
    p.extend_from_slice(&vy.to_be_bytes());
    p.extend_from_slice(&vz.to_be_bytes());
    p
}

/// Decode the carried stack a container-click packet claims to hold. The tail
/// after the click mode is `changedSlots` (varint-sized map of slot -> stack)
/// followed by the carried `ItemStack`. Map entries embed full stacks whose
/// boundaries need the component-aware decoder, so a non-empty map yields
/// `None` and callers fall back to a conservative rule.
fn click_carried_stack(tail: &[u8]) -> Option<Stack> {
    let mut p = tail;
    if read_protocol_i32(&mut p)? != 0 {
        return None;
    }
    let mut buf = vec![0u8, 0u8];
    buf.extend_from_slice(p);
    slot_read_wire(&buf).map(|(_, st)| st)
}

/// Hurt Animation (`0x24`): entity id + the hurt direction yaw vanilla computes
/// in `ServerPlayer.indicateDamage` (`getHurtDir`).
fn hurt_anim_yaw(eid: i32, yaw: f32) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, eid as i64);
    p.extend_from_slice(&yaw.to_be_bytes());
    p
}

/// Entity Event (`0x1f`): i32 entity id + i8 status. Status 3 is the death
/// event vanilla broadcasts from `LivingEntity.die`.
fn entity_event_pkt(eid: i32, status: i8) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&eid.to_be_bytes());
    p.push(status as u8);
    p
}

/// Vanilla `LivingEntity.knockback(0.4, dx, dz)` for a bare-hand player hit,
/// reduced to the `SetEntityMotion` shorts. `pp`/`vp` are attacker/victim
/// `(x, y, z, yaw, pitch, on_ground)`. Player knockback resistance is always 0
/// here, so the strength is the vanilla 0.4 constant.
fn vanilla_knockback(
    vp: &(f64, f64, f64, f32, f32, bool),
    pp: &(f64, f64, f64, f32, f32, bool),
) -> (i16, i16, i16) {
    let strength = 0.4f64;
    let (mut dx, mut dz) = (pp.0 - vp.0, pp.2 - vp.2);
    if dx * dx + dz * dz < 1e-10 {
        // Vanilla rerolls a random direction here; a deterministic zero keeps
        // the packet stable for the degenerate same-position case.
        dx = 0.0;
        dz = 0.0;
    } else {
        let len = (dx * dx + dz * dz).sqrt();
        dx /= len;
        dz /= len;
    }
    // Players have no server-side velocity state; a settled on-ground body
    // carries vanilla's one-tick gravity residue (-0.08 * 0.98 drag).
    let old_y = if vp.5 { -0.08 * 0.98 } else { 0.0 };
    let nx = 0.0f64 / 2.0 - dx * strength;
    let ny = if vp.5 {
        (old_y / 2.0 + strength).min(0.4)
    } else {
        old_y
    };
    let nz = 0.0f64 / 2.0 - dz * strength;
    (
        (nx.clamp(-3.9, 3.9) * 8000.0) as i16,
        (ny.clamp(-3.9, 3.9) * 8000.0) as i16,
        (nz.clamp(-3.9, 3.9) * 8000.0) as i16,
    )
}

fn spawn_mob(m: &Mob) -> Vec<u8> {
    let (x, y, z, yaw, pitch) = *m.pos.lock().unwrap();
    let mut p = Vec::new();
    write_varint(&mut p, m.eid as i64);
    p.extend_from_slice(&m.uuid);
    write_varint(&mut p, m.etype);
    p.extend_from_slice(&x.to_be_bytes());
    p.extend_from_slice(&y.to_be_bytes());
    p.extend_from_slice(&z.to_be_bytes());
    spawn_angles(&mut p, yaw, pitch);
    write_varint(&mut p, 0);
    p.extend_from_slice(&[0u8; 6]);
    p
}
fn spawn_entity_pkt(
    eid: i32,
    etype: u32,
    x: f64,
    y: f64,
    z: f64,
    yaw: f32,
    pitch: f32,
    data: i64,
) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, eid as i64);
    p.extend_from_slice(&[0u8; 16]);
    write_varint(&mut p, etype as i64);
    p.extend_from_slice(&x.to_be_bytes());
    p.extend_from_slice(&y.to_be_bytes());
    p.extend_from_slice(&z.to_be_bytes());
    spawn_angles(&mut p, yaw, pitch);
    write_varint(&mut p, data);
    p.extend_from_slice(&[0u8; 6]);
    p
}
/// `minecraft:falling_block` (entity type 40) carries its block state as the spawn packet's
/// object data, so the client renders the right block while it falls.
fn spawn_falling_pkt(eid: i32, state: u32, x: f64, y: f64, z: f64) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, eid as i64);
    p.extend_from_slice(&[0u8; 16]);
    write_varint(&mut p, 40);
    p.extend_from_slice(&x.to_be_bytes());
    p.extend_from_slice(&y.to_be_bytes());
    p.extend_from_slice(&z.to_be_bytes());
    p.extend_from_slice(&[0u8; 3]);
    write_varint(&mut p, state as i64);
    p.extend_from_slice(&[0u8; 6]);
    p
}

// sound/particle id subsets (minecraft-data 1.21.1 registries, unnamespaced)
// sound_effect packet (0x68): named sound at a position, ambient cat
fn sound_pkt_named(name: &str, x: f64, y: f64, z: f64) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, sound_id(name).unwrap_or(0) as i64 + 1);
    write_varint(&mut p, 0);
    p.extend_from_slice(&((x * 8.0) as i32).to_be_bytes());
    p.extend_from_slice(&((y * 8.0) as i32).to_be_bytes());
    p.extend_from_slice(&((z * 8.0) as i32).to_be_bytes());
    p.extend_from_slice(&1.0f32.to_be_bytes());
    p.extend_from_slice(&1.0f32.to_be_bytes());
    p.extend_from_slice(&0i64.to_be_bytes());
    p
}
fn sound_id(n: &str) -> Option<i32> {
    for (k, v) in [
        ("entity.player.levelup", 1115),
        ("entity.experience_orb.pickup", 502),
        ("ui.toast.challenge_complete", 1434),
        ("entity.generic.explode", 577),
        ("entity.lightning_bolt.thunder", 783),
        ("block.chest.open", 303),
        ("entity.item.pickup", 763),
        ("entity.arrow.hit_player", 84),
        ("block.anvil.land", 51),
        ("block.note_block.pling", 995),
        ("block.note_block.bell", 989),
        ("ambient.cave", 8),
        ("entity.zombie.ambient", 1586),
        ("entity.villager.yes", 1461),
        ("entity.enderman.teleport", 483),
        ("entity.player.hurt", 1110),
        ("entity.zombie.death", 1591),
        ("entity.skeleton.ambient", 1256),
        ("entity.creeper.primed", 375),
        ("entity.spider.ambient", 1344),
        ("block.grass.break", 629),
        ("block.stone.break", 1362),
        ("entity.firework_rocket.launch", 510),
        ("entity.wither.spawn", 1548),
        ("entity.ender_dragon.death", 470),
        ("entity.egg.throw", 456),
        ("entity.firework_rocket.blast", 506),
        ("block.portal.travel", 1133),
        ("entity.item_frame.place", 759),
        ("block.note_block.harp", 993),
    ] {
        if k == n {
            return Some(v);
        }
    }
    None
}
fn particle_id(n: &str) -> Option<i32> {
    for (k, v) in [
        ("flame", 31),
        ("heart", 42),
        ("crit", 5),
        ("happy_villager", 40),
        ("explosion", 22),
        ("smoke", 56),
        ("end_rod", 19),
        ("totem_of_undying", 62),
        ("portal", 54),
        ("dragon_breath", 7),
        ("angry_villager", 0),
        ("note", 52),
        ("snowflake", 88),
        ("soul", 38),
        ("campfire_cosy_smoke", 71),
        ("enchanted_hit", 17),
        ("large_smoke", 49),
        ("cloud", 4),
        ("splash", 64),
        ("rain", 55),
        ("composter", 41),
        ("firework", 29),
        ("flash", 39),
        ("glow", 94),
        ("sculk_soul", 34),
        ("sonic_boom", 27),
        ("cherry_leaves", 33),
        ("poof", 53),
    ] {
        if k == n {
            return Some(v);
        }
    }
    None
}

fn resource_pack_pkt() -> Option<Vec<u8>> {
    let (url, hash) = pack_info().as_ref()?;
    let mut p = Vec::new();
    p.extend_from_slice(&[0xabu8; 16]); // uuid
    write_str(&mut p, url);
    write_str(&mut p, hash);
    p.push(resource_pack_required() as u8);
    p.push(0); // no prompt
    Some(p)
}
/// Whether the configured resource pack is mandatory. The server config's
/// `resource_pack_required` key boots into `MC_SPINE_RESOURCE_PACK_REQUIRED`;
/// a Vanilla `server.properties` says `require-resource-pack`. Either marks the
/// pack required.
fn resource_pack_required() -> bool {
    fn truthy(value: &str) -> bool {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        )
    }
    std::env::var("MC_SPINE_RESOURCE_PACK_REQUIRED").is_ok_and(|v| truthy(&v))
        || server_properties_lookup("require-resource-pack").is_some_and(|v| truthy(&v))
}

// ---------- config blob replay ----------
// Registry packet order in the vanilla 1.21.1 configuration capture.
const SYNCED_REGISTRIES: [&str; 11] = [
    "worldgen/biome",
    "chat_type",
    "trim_pattern",
    "trim_material",
    "wolf_variant",
    "painting_variant",
    "dimension_type",
    "damage_type",
    "banner_pattern",
    "enchantment",
    "jukebox_song",
];

/// Build Configuration 0x07 payloads (starting at the registry ID) from pack
/// files in low-to-high priority order. Directory and ZIP readers supply paths
/// relative to the pack root. A partial input emits only its populated registries.
fn build_registry_data_payloads<'a>(
    packs: impl IntoIterator<Item = impl IntoIterator<Item = (&'a str, &'a [u8])>>,
) -> Result<Vec<Vec<u8>>, String> {
    let mut registries = std::collections::BTreeMap::<_, std::collections::BTreeMap<_, _>>::new();
    for (priority, files) in packs.into_iter().enumerate() {
        for (file, json) in files {
            let Some((namespace, relative)) = file
                .strip_prefix("data/")
                .and_then(|path| path.split_once('/'))
            else {
                continue;
            };
            let Some(relative) = relative.strip_suffix(".json") else {
                continue;
            };
            let Some((kind, path)) = SYNCED_REGISTRIES.iter().find_map(|&kind| {
                let path = relative.strip_prefix(kind)?.strip_prefix('/')?;
                Some((kind, path))
            }) else {
                continue;
            };
            // Vanilla ResourceLocation orders by path, then by namespace.
            let previous = registries
                .entry(kind)
                .or_default()
                .insert((path, namespace), (priority, file, json));
            if previous.is_some_and(|(previous_priority, _, _)| previous_priority == priority) {
                return Err(format!("duplicate registry file {file} in one datapack"));
            }
        }
    }

    let mut payloads = Vec::new();
    for kind in SYNCED_REGISTRIES {
        let Some(entries) = registries.remove(kind) else {
            continue;
        };
        let mut payload = Vec::new();
        write_str(&mut payload, &format!("minecraft:{kind}"));
        let count = i32::try_from(entries.len()).map_err(|_| "too many registry entries")?;
        write_varint(&mut payload, i64::from(count));
        for ((path, namespace), (_, file, json)) in entries {
            let location = brigadier_wire::ResourceLocation::new(namespace, path)
                .map_err(|_| format!("invalid registry entry path {file}"))?;
            let id = location.to_string();
            if id.len() > 32_767 || path.split('/').any(|part| matches!(part, "" | "." | "..")) {
                return Err(format!("invalid registry entry path {file}"));
            }
            let mut tag =
                nbt::registry_json_to_nbt(json).map_err(|err| format!("{file}: {err}"))?;
            if kind == "worldgen/biome" {
                // Biome's network codec carries only the climate and effects.
                if let nbt::Tag::Compound(fields) = &mut tag {
                    for field in [
                        "carvers",
                        "features",
                        "spawn_costs",
                        "spawners",
                        "creature_spawn_probability",
                    ] {
                        fields.remove(&nbt::NbtString::text(field));
                    }
                }
            }
            write_str(&mut payload, &id);
            payload.push(1); // Optional NBT is present for every entry.
            payload.extend(nbt::write_network_nbt(&tag).map_err(|err| format!("{file}: {err}"))?);
        }
        payloads.push(payload);
    }
    Ok(payloads)
}

fn registry_pack_files(
    base: &std::path::Path,
    zip: bool,
) -> std::io::Result<Vec<(String, Vec<u8>)>> {
    if zip {
        let mut bytes = Vec::new();
        std::fs::File::open(base)?
            .take(ZIP_MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        return (bytes.len() <= ZIP_MAX_BYTES)
            .then(|| zip_datapack_entries(&bytes))
            .flatten()
            .ok_or_else(|| std::io::Error::other("invalid or unsupported registry datapack ZIP"));
    }
    let data = base.join("data");
    let namespaces = match std::fs::read_dir(data) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut files = Vec::new();
    for namespace in namespaces {
        let namespace = namespace?;
        if !namespace.file_type()?.is_dir() {
            continue;
        }
        for kind in SYNCED_REGISTRIES {
            let mut dirs = vec![namespace.path().join(kind)];
            while let Some(dir) = dirs.pop() {
                let entries = match std::fs::read_dir(dir) {
                    Ok(entries) => entries,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                };
                for entry in entries {
                    let entry = entry?;
                    let file_type = entry.file_type()?;
                    let path = entry.path();
                    if file_type.is_dir() {
                        dirs.push(path);
                    } else if file_type.is_file() && path.extension().is_some_and(|e| e == "json") {
                        if let Some(name) = path.strip_prefix(base).ok().and_then(|p| p.to_str()) {
                            files.push((name.replace('\\', "/"), std::fs::read(path)?));
                        }
                    }
                }
            }
        }
    }
    Ok(files)
}

fn load_registry_overlays(base: &std::path::Path) -> Result<Vec<Vec<u8>>, String> {
    let mut sources = datapack_sources(base);
    // The built-in pack is the base. User packs follow in the server's pack order.
    sources.sort_by_key(|(name, _, _)| name != "vanilla");
    let disabled = disabled_datapacks().lock().unwrap().clone();
    let mut packs = Vec::new();
    for (name, path, zip) in sources {
        if !disabled.contains(&name) {
            packs
                .push(registry_pack_files(&path, zip).map_err(|error| format!("{name}: {error}"))?);
        }
    }
    build_registry_data_payloads(packs.iter().map(|files| {
        files
            .iter()
            .map(|(name, data)| (name.as_str(), data.as_slice()))
    }))
}

fn captured_config_packets() -> Option<Vec<(i64, Vec<u8>)>> {
    let data = std::fs::read("config_packets.bin").ok()?;
    let mut packets = Vec::new();
    let mut input = data.as_slice();
    while !input.is_empty() {
        let len = usize::try_from(read_protocol_i32(&mut input)?).ok()?;
        let (packet, rest) = input.split_at_checked(len)?;
        let id = read_protocol_i32(&mut &packet[..])?;
        packets.push((i64::from(id), packet.to_vec()));
        input = rest;
    }
    Some(packets)
}

fn registry_wire_name<'a>(input: &mut &'a [u8]) -> Option<&'a str> {
    let len = usize::try_from(read_protocol_i32(input)?).ok()?;
    let (name, rest) = input.split_at_checked(len)?;
    *input = rest;
    std::str::from_utf8(name).ok()
}

fn config_registry_header(packet: &[u8]) -> Option<(&str, &[u8])> {
    let mut input = packet;
    (read_protocol_i32(&mut input)? == 0x07).then_some(())?;
    let name = registry_wire_name(&mut input)?;
    Some((name, input))
}

fn registry_wire_entries(mut input: &[u8]) -> Option<Vec<(&str, &[u8])>> {
    let count = read_protocol_i32(&mut input)?;
    if !(0..=4096).contains(&count) {
        return None;
    }
    let mut entries = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let name = registry_wire_name(&mut input)?;
        let entry = input;
        let (&has_nbt, body) = input.split_first()?;
        let mut end = 0;
        match has_nbt {
            0 => {}
            1 if nbt_wire_skip(body, &mut end, 0) => {}
            _ => return None,
        }
        let (entry, rest) = entry.split_at_checked(1 + end)?;
        entries.push((name, entry));
        input = rest;
    }
    input.is_empty().then_some(entries)
}

fn merge_registry_overlays(config: &mut [(i64, Vec<u8>)], overlays: &[Vec<u8>]) -> Option<()> {
    let mut pending = HashMap::new();
    for payload in overlays {
        let mut body = payload.as_slice();
        let name = registry_wire_name(&mut body)?;
        pending.insert(name, registry_wire_entries(body)?);
    }
    let mut replacements = Vec::new();
    for (index, (_, packet)) in config.iter().enumerate().filter(|(_, (id, _))| *id == 0x07) {
        let (name, body) = config_registry_header(packet)?;
        let Some(entries) = pending.remove(name) else {
            continue;
        };
        let mut overrides: HashMap<_, _> = entries.iter().copied().collect();
        let mut ordered = registry_wire_entries(body)?;
        for (id, payload) in &mut ordered {
            if let Some(replacement) = overrides.remove(id) {
                *payload = replacement;
            }
        }
        // Keep baseline numeric IDs stable for Update Tags, chunk biome IDs and
        // dimension IDs. New entries are appended in the builder's order.
        ordered.extend(
            entries
                .into_iter()
                .filter(|(id, _)| overrides.contains_key(id)),
        );
        let mut merged = vec![0x07];
        write_str(&mut merged, name);
        write_varint(&mut merged, ordered.len() as i64);
        for (id, payload) in ordered {
            write_str(&mut merged, id);
            merged.extend_from_slice(payload);
        }
        replacements.push((index, merged));
    }
    if !pending.is_empty() {
        return None;
    }
    for (index, packet) in replacements {
        config[index].1 = packet;
    }
    Some(())
}

fn load_config_packets() -> Vec<(i64, Vec<u8>)> {
    let Some(mut config) = captured_config_packets() else {
        eprintln!(
            "[spine] missing or invalid config_packets.bin. {}",
            worldgen::VANILLA_DATA_MISSING
        );
        return Vec::new();
    };
    match load_registry_overlays(std::path::Path::new(DATAPACK_DIRECTORY)) {
        Ok(overlays) => {
            if merge_registry_overlays(&mut config, &overlays).is_none() {
                eprintln!("[spine] unable to merge datapack registries into config_packets.bin");
            }
        }
        Err(error) => eprintln!("[spine] unable to load datapack registries: {error}"),
    }
    append_custom_dimension_types(&mut config);
    config
}

/// The `minecraft:dimension_type` entry NBT a registered custom dimension declares,
/// in the field order the vanilla registry writer emits.
fn custom_dimension_type_nbt(spec: &worldgen::DimensionSpec) -> nbt::Tag {
    let b = |v: bool| nbt::Tag::Byte(v as i8);
    let t = nbt::NbtString::text;
    let mut fields = std::collections::BTreeMap::new();
    fields.insert(t("piglin_safe"), b(spec.piglin_safe));
    fields.insert(t("has_raids"), b(spec.has_raids));
    fields.insert(
        t("monster_spawn_light_level"),
        nbt::Tag::Int(spec.monster_spawn_light_level),
    );
    fields.insert(
        t("monster_spawn_block_light_limit"),
        nbt::Tag::Int(spec.monster_spawn_block_light_limit),
    );
    fields.insert(t("natural"), b(spec.natural));
    fields.insert(
        t("ambient_light"),
        nbt::Tag::Float(spec.ambient_light.to_bits()),
    );
    if let Some(fixed_time) = spec.fixed_time {
        fields.insert(t("fixed_time"), nbt::Tag::Long(fixed_time));
    }
    fields.insert(
        t("infiniburn"),
        nbt::Tag::String(t(&spec.infiniburn)),
    );
    fields.insert(t("respawn_anchor_works"), b(spec.respawn_anchor_works));
    fields.insert(t("has_skylight"), b(spec.has_skylight));
    fields.insert(t("bed_works"), b(spec.bed_works));
    fields.insert(t("effects"), nbt::Tag::String(t(&spec.effects)));
    fields.insert(t("min_y"), nbt::Tag::Int(spec.min_y));
    fields.insert(t("height"), nbt::Tag::Int(spec.height));
    fields.insert(t("logical_height"), nbt::Tag::Int(spec.logical_height));
    fields.insert(
        t("coordinate_scale"),
        nbt::Tag::Double(spec.coordinate_scale.to_bits()),
    );
    fields.insert(t("ultrawarm"), b(spec.ultrawarm));
    fields.insert(t("has_ceiling"), b(spec.has_ceiling));
    nbt::Tag::Compound(fields)
}

/// Append the registered custom `dimension_type` entries to the merged `0x07` registry
/// packet — a datapack may already carry its type file (merged by the overlays), in
/// which case only the wire-id table learns its position — and record every entry's
/// wire index so `dim_type_wire_id` can answer for custom dimensions.
fn append_custom_dimension_types(config: &mut [(i64, Vec<u8>)]) {
    let specs = worldgen::custom_dimension_specs();
    if specs.is_empty() {
        return;
    }
    for (_, packet) in config.iter_mut().filter(|(id, _)| *id == 0x07) {
        let Some((name, body)) = config_registry_header(packet) else {
            continue;
        };
        if name != "minecraft:dimension_type" {
            continue;
        }
        let Some(entries) = registry_wire_entries(body) else {
            continue;
        };
        let mut ids: HashMap<String, i32> = entries
            .iter()
            .enumerate()
            .map(|(index, (entry, _))| (entry.to_string(), index as i32))
            .collect();
        let mut appended = Vec::new();
        for spec in &specs {
            if ids.contains_key(spec.type_key) {
                continue;
            }
            let Ok(nbt) = nbt::write_network_nbt(&custom_dimension_type_nbt(spec)) else {
                eprintln!("[spine] cannot encode dimension_type {}", spec.type_key);
                continue;
            };
            let mut entry = Vec::new();
            entry.push(1); // Optional NBT present.
            entry.extend_from_slice(&nbt);
            let index = (entries.len() + appended.len()) as i32;
            ids.insert(spec.type_key.to_string(), index);
            appended.push((spec.type_key, entry));
        }
        {
            let mut table = dim_type_entry_ids().lock().unwrap();
            for spec in &specs {
                if let Some(&index) = ids.get(spec.type_key) {
                    table.insert(spec.type_key, index);
                }
            }
        }
        if appended.is_empty() {
            continue;
        }
        let count = entries.len() + appended.len();
        let mut merged = vec![0x07];
        write_str(&mut merged, name);
        write_varint(&mut merged, count as i64);
        for (id, payload) in entries {
            write_str(&mut merged, id);
            merged.extend_from_slice(payload);
        }
        for (id, payload) in appended {
            write_str(&mut merged, id);
            merged.extend_from_slice(&payload);
        }
        *packet = merged;
    }
}

/// `type_key -> wire index` for the `minecraft:dimension_type` entries registered
/// custom dimensions reference, published by `append_custom_dimension_types` when the
/// config blob is built.
fn dim_type_entry_ids() -> &'static Mutex<HashMap<&'static str, i32>> {
    static IDS: OnceLock<Mutex<HashMap<&'static str, i32>>> = OnceLock::new();
    IDS.get_or_init(Mutex::default)
}

fn player_attack_damage_wire_id(config: &[(i64, Vec<u8>)]) -> Option<i32> {
    let mut registry = None;
    for (_, packet) in config.iter().filter(|(id, _)| *id == 0x07) {
        let (name, body) = config_registry_header(packet)?;
        if name == "minecraft:damage_type" && registry.replace(body).is_some() {
            return None;
        }
    }
    let mut matches = registry_wire_entries(registry?)?
        .into_iter()
        .enumerate()
        .filter(|(_, (name, _))| *name == "minecraft:player_attack");
    let (index, _) = matches.next()?;
    matches.next().is_none().then_some(index as i32)
}

const SERVER_VIEW_DISTANCE: i32 = 8;
const SERVER_SIMULATION_DISTANCE: i32 = SERVER_VIEW_DISTANCE;

/// S2C `0x55` SetChunkCacheRadius + `0x62` SetSimulationDistance: sent right
/// after Join Game and whenever the client's effective view distance changes.
fn send_view_distance(s: &mut TcpStream, view: i32) {
    let mut p = Vec::new();
    write_varint(&mut p, i64::from(view));
    let _ = send_packet(s, 0x55, p);
    let mut p = Vec::new();
    write_varint(&mut p, i64::from(SERVER_SIMULATION_DISTANCE));
    let _ = send_packet(s, 0x62, p);
}

/// Decode protocol 767 Client Information; view distance is a signed Byte.
/// The connection bounds the requested value, not the packet decoder.
fn client_info_view_distance(body: &[u8]) -> Option<i32> {
    let mut p = body;
    let len = usize::try_from(read_protocol_i32(&mut p)?).ok()?;
    if len > 64 || p.len() < len {
        return None;
    }
    let locale = std::str::from_utf8(&p[..len]).ok()?;
    if locale.encode_utf16().count() > 16 {
        return None;
    }
    p = &p[len..];
    let view = *p.first()? as i8 as i32;
    p = &p[1..];
    if !(0..=2).contains(&read_protocol_i32(&mut p)?) {
        return None;
    } // chat mode
    if p.len() < 2 {
        return None;
    }
    p = &p[2..]; // chat colours and skin parts
    if !(0..=1).contains(&read_protocol_i32(&mut p)?) {
        return None;
    } // main hand
    if p.len() != 2 {
        return None;
    } // filtering and server listings
    Some(view)
}

/// The number of chunks the current view window can hold.
fn window_batch(view: i32) -> usize {
    let side = (2 * view + 1).max(1) as usize;
    side * side
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ChunkWindow {
    center: (i32, i32),
    radius: i32,
}

impl ChunkWindow {
    fn at(x: f64, z: f64, radius: i32) -> Self {
        Self {
            center: (spatial_chunk_coord(x), spatial_chunk_coord(z)),
            radius,
        }
    }

    fn contains(self, (x, z): (i32, i32)) -> bool {
        x.abs_diff(self.center.0).max(z.abs_diff(self.center.1)) <= self.radius as u32
    }

    fn leaving(self, previous: Option<Self>, sent: &HashSet<(i32, i32)>) -> Vec<(i32, i32)> {
        if let Some(old) = previous.filter(|old| old.radius == self.radius) {
            let (_, removed) = chunk_view_diff(old.center, self.center, self.radius as u32);
            return removed.into_iter().filter(|c| sent.contains(c)).collect();
        }
        sent.iter()
            .copied()
            .filter(|&c| !self.contains(c))
            .collect()
    }

    fn unsent(self, sent: &HashSet<(i32, i32)>) -> impl Iterator<Item = (i32, i32)> + '_ {
        let (cx, cz) = self.center;
        let r = self.radius;
        (cx.saturating_sub(r)..=cx.saturating_add(r))
            .flat_map(move |x| (cz.saturating_sub(r)..=cz.saturating_add(r)).map(move |z| (x, z)))
            .filter(move |c| !sent.contains(c))
    }
}

/// Replace the full light lists from map_chunk's cached payload with sparse
/// lists. Block and biome palettes still carry one record per vertical section.
fn cull_chunk_light(packet: &mut Vec<u8>, dim: DimId) {
    const ARRAY_WIRE_SIZE: usize = 2 + 2048;
    let all = dim.light_mask_longs();
    let words = all.len();
    let block_count = dim.light_sections();
    let sky_count = if worldgen::has_skylight(dim) {
        block_count
    } else {
        0
    };
    // VarInt-coded `long[]` length; every count here stays under 128 except a
    // very tall dimension's array counts.
    let varint_len = |v: usize| ((usize::BITS - v.leading_zeros()) as usize).div_ceil(7).max(1);
    // two full bitsets, two empty ones, the two array-count VarInts, then the
    // two counted array lists.
    let suffix_size = 2 * (1 + 8 * words)
        + 2
        + varint_len(sky_count)
        + varint_len(block_count)
        + (sky_count + block_count) * ARRAY_WIRE_SIZE;
    let Some(start) = packet.len().checked_sub(suffix_size) else {
        return;
    };
    let mut light = &packet[start..];
    let read_mask = |light: &mut &[u8]| -> Option<Vec<u64>> {
        let count = usize::try_from(read_protocol_i32(light)?).ok()?;
        if count * 8 > light.len() {
            return None;
        }
        let (raw, rest) = light.split_at(count * 8);
        *light = rest;
        Some(
            raw.chunks_exact(8)
                .map(|w| u64::from_be_bytes(w.try_into().unwrap()))
                .collect(),
        )
    };
    // The two full-coverage masks: sky is `all` for a lit dimension and an
    // all-zero bitset (zero words, or padded zero words) for a dark one; the
    // block mask is always `all`. Anything else is not a full-coverage packet
    // this transform understands.
    let Some(sky_mask) = read_mask(&mut light) else {
        return;
    };
    let sky_ok = if sky_count == 0 {
        sky_mask.iter().all(|w| *w == 0)
    } else {
        sky_mask == all
    };
    let block_ok = read_mask(&mut light).is_some_and(|m| m == all);
    if !sky_ok || !block_ok {
        return;
    }
    if !light.starts_with(&[0, 0]) {
        return;
    }
    light = &light[2..];
    let mut lists = [Vec::with_capacity(sky_count), Vec::with_capacity(block_count)];
    for (arrays, count) in lists.iter_mut().zip([sky_count, block_count]) {
        if read_protocol_i32(&mut light) != Some(count as i32) {
            return;
        }
        for _ in 0..count {
            if read_protocol_i32(&mut light) != Some(2048) || light.len() < 2048 {
                return;
            }
            let (array, rest) = light.split_at(2048);
            arrays.push(array);
            light = rest;
        }
    }
    if !light.is_empty() {
        return;
    }
    let bands = lists.each_ref().map(|arrays| {
        let nonempty: Vec<_> = arrays
            .iter()
            .map(|section| section[0] != 0 || uniform_section(*section) != Some(0))
            .collect();
        // Full light-engine band, including the two boundary sections. A
        // nonzero light array can belong to an all-air block section.
        light_sections_in_band(&nonempty, 0, arrays.len().saturating_sub(1))
    });
    let [(sky, empty_sky), (block, empty_block)] = std::array::from_fn(|layer| {
        let mut present = vec![0u64; lists[layer].len().div_ceil(64)];
        for &i in &bands[layer] {
            present[i / 64] |= 1 << (i % 64);
        }
        let full = if lists[layer].len() == block_count {
            all.clone()
        } else {
            Vec::new()
        };
        let empty = full
            .iter()
            .zip(&present)
            .map(|(a, p)| a ^ p)
            .collect::<Vec<u64>>();
        (present, empty)
    });
    let array_bytes = (bands[0].len() + bands[1].len()) * ARRAY_WIRE_SIZE;
    let mut sparse = Vec::with_capacity(4 * (1 + 8 * words) + 2 + array_bytes);
    for mask in [&sky, &block, &empty_sky, &empty_block] {
        write_light_bitset(&mut sparse, mask);
    }
    for (band, arrays) in bands.iter().zip(&lists) {
        write_varint(&mut sparse, band.len() as i64);
        for &i in band {
            write_varint(&mut sparse, 2048);
            sparse.extend_from_slice(arrays[i]);
        }
    }
    packet.truncate(start);
    packet.extend_from_slice(&sparse);
}

fn chunk_packet_cache() -> &'static Mutex<connection_limits::ChunkPacketCache> {
    static CACHE: OnceLock<Mutex<connection_limits::ChunkPacketCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(connection_limits::ChunkPacketCache::new()))
}

/// FNV-1a combine step, matching `map_chunk_sig`'s combiner.
fn chunk_packet_mix(h: u64, v: u64) -> u64 {
    (h ^ v).wrapping_mul(0x0000_0100_0000_01b3)
}

/// The revision a shared `0x27` frame is gated on: every input the sent bytes
/// can carry. The 3x3 `chunk_versions` tuple moves on any `set_block*` write in
/// the chunk or a neighbour (block edits and the light updates they cause),
/// `map_chunk_sig` covers signs, Overworld spawners, the neighbour generated
/// emitters in `gen` and the settled light revision, and the fingerprint
/// covers the live block entities `splice_chunk_block_entities` merges in.
fn chunk_packet_revision(
    versions: &[u64; 9],
    sig: u64,
    block_entities: &[ChunkBlockEntity],
) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &v in versions {
        h = chunk_packet_mix(h, v);
    }
    h = chunk_packet_mix(h, sig);
    for ((x, y, z), wire_id, nbt) in block_entities {
        for v in [*x as u64, *y as u64, *z as u64, *wire_id as u64] {
            h = chunk_packet_mix(h, v);
        }
        for &b in nbt {
            h = chunk_packet_mix(h, u64::from(b));
        }
    }
    h
}

/// The compression threshold `s` negotiated, if any. Read once per
/// `stream_chunks` call: the frames the cache hands out are shared, so the
/// per-packet `send_packet` lookup is not repeated per chunk.
fn socket_compression(s: &TcpStream) -> Option<usize> {
    SOCKET_WIRE.get().and_then(|states| {
        states
            .lock()
            .unwrap()
            .get(&socket_key(s))?
            .compression
    })
}

/// Write an already-framed packet (bytes shared from `ChunkPacketCache`) with
/// `write_frame`'s failure contract: a partial write kills the socket.
fn write_shared_frame(s: &mut TcpStream, frame: &[u8]) -> std::io::Result<()> {
    let encrypted;
    let frame = match conn_cipher(s) {
        Some(cipher) => {
            let mut v = frame.to_vec();
            cipher.tx.lock().unwrap().encrypt(&mut v);
            encrypted = v;
            &encrypted[..]
        }
        None => frame,
    };
    let result = s.write_all(frame);
    if result.is_err() {
        let _ = s.shutdown(std::net::Shutdown::Both);
    }
    result
}

/// The shared `0x27` frame for `(dim, cx, cz)` at its current world revision.
/// On a miss the packet is rebuilt exactly as before — `map_chunk`, the live
/// block-entity splice, the light cull — then stored under the revision the
/// build started from only when those inputs held to the end; a mid-build edit
/// gets correct bytes that are simply not cached.
///
/// `occluded` selects the anti-xray lane: `true` is the surface player's
/// masked chunk, `false` the real one. The lanes are separate cache keys, so
/// every surface reader shares one `Arc` and every underground reader another.
fn cached_chunk_frame(
    compression: Option<usize>,
    gen: &mut GenSources,
    dim: DimId,
    cx: i32,
    cz: i32,
    occluded: bool,
) -> Arc<Vec<u8>> {
    let key = (dim, cx, cz, occluded);
    let generation = current_gen_seq();
    // `live_chunk_block_entities` iterates a map; sort so the spliced bytes
    // (and their fingerprint) are deterministic across readers.
    let mut entities = live_chunk_block_entities(dim, cx, cz);
    // Anti-xray: do not leak block-entity NBT (chests, barrels, suspicious sand/gravel
    // LootTableSeed, vaults, etc.) for fully buried cells until exposed.
    if anti_xray_enabled() {
        entities.retain(|&((x, y, z), _, _)| block_exposed_dim(dim, x, y, z));
    }
    // The occluded lane strips every live block entity under the ceiling too:
    // a buried beacon, spawner or chest must not reach a surface client at all.
    if occluded {
        if let Some(ceil) = underground_occlude_ceil(dim, cx, cz) {
            entities.retain(|&((_, y, _), _, _)| y >= ceil);
        }
    }
    entities.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
    let versions = chunk_versions_3x3(dim, cx, cz);
    let revision = chunk_packet_revision(&versions, map_chunk_sig(dim, cx, cz, gen), &entities);
    if let Some((frame, emitters)) = chunk_packet_cache()
        .lock()
        .unwrap()
        .frame(generation, key, revision, compression)
    {
        // A hit restores the chunk's own emitters into `gen` exactly as a
        // rebuild would; `light_dirty_for_dim` reads them below.
        gen.insert((cx, cz), emitters);
        return frame;
    }
    let mut packet = map_chunk_lane(cx, cz, dim, gen, occluded);
    let emitters = gen.get(&(cx, cz)).cloned().unwrap_or_default();
    let still_current = chunk_packet_revision(
        &chunk_versions_3x3(dim, cx, cz),
        map_chunk_sig(dim, cx, cz, gen),
        &entities,
    ) == revision;
    splice_chunk_block_entities(&mut packet, entities);
    cull_chunk_light(&mut packet, dim);
    if !still_current {
        return connection_limits::ChunkPacketCache::frame_uncached(&packet, compression);
    }
    let mut cache = chunk_packet_cache().lock().unwrap();
    cache.insert(generation, key, revision, emitters, packet);
    cache
        .frame(generation, key, revision, compression)
        .map(|(frame, _)| frame)
        .expect("a freshly inserted chunk packet must frame")
}

fn stream_chunks(
    s: &mut TcpStream,
    sent: &mut std::collections::HashSet<(i32, i32)>,
    gen: &mut GenSources,
    ack: &mut ChunkFeedback,
    dim: DimId,
    window: ChunkWindow,
    yaw: f32,
    // the centre is announced when the window moves (spawn, view request,
    // player crossing a chunk) - not on every batch, or a client pacing its own
    // batches would see the centre move while nothing did
    announce_center: bool,
    // The first batch is sent before the shared Play writer has been registered.
    initial_delivery: Option<&mut connection_limits::ChunkDelivery>,
    // Anti-xray lane for this player: `true` masks deep sections to solid host
    // rock for a surface player, `false` sends the real chunk. Decided by
    // `update_underground_lane` before the stream, never per chunk.
    occluded: bool,
) {
    let tick = TICK_IDX.load(Ordering::Relaxed);
    let select = |delivery: &mut connection_limits::ChunkDelivery| {
        let previous = delivery.window();
        let news = delivery.next_batch(window, sent, yaw, ack, tick);
        (previous, news)
    };
    let (previous, news) = if let Some(delivery) = initial_delivery {
        select(delivery)
    } else {
        with_play_wire(s, |play| select(&mut play.chunk_delivery))
            .unwrap_or_else(|| select(&mut connection_limits::ChunkDelivery::default()))
    };
    // chunks leaving the window get an explicit unload — vanilla keeps
    // client memory bounded and the set now means exactly "chunks the
    // client holds" (updates must only target holders).
    for c in window.leaving(previous, sent) {
        let mut p = Vec::with_capacity(8);
        p.extend_from_slice(&c.1.to_be_bytes()); // wire order: chunkZ, chunkX
        p.extend_from_slice(&c.0.to_be_bytes());
        if send_packet(s, 0x21, p).is_err() {
            return;
        }
        sent.remove(&c);
        gen.remove(&c);
    }
    // The centre update is not part of a batch, but it is the answer a view
    // request waits for even when nothing new is to be sent.
    if announce_center || previous.is_some_and(|old| old != window) {
        let mut vp = Vec::new();
        write_varint(&mut vp, i64::from(window.center.0));
        write_varint(&mut vp, i64::from(window.center.1));
        if send_packet(s, 0x54, vp).is_err() {
            return;
        }
    }
    if news.is_empty() {
        return;
    }
    let batch = news.len();
    // Terrain is pure per chunk, so a group of the batch is built off the lock before the
    // send loop touches it; everything after it (features, overlays, encoding) stays
    // sequential in the order the client is served. The group size is bounded so the base
    // rows in `terr_c` cannot outgrow their store.
    if send_packet(s, 0x0d, vec![]).is_err() {
        return;
    }
    let mut dirty: std::collections::HashSet<(i32, i32)> = std::collections::HashSet::new();
    let delivery_workers = delivery_prefetch_jobs();
    let delivery_round = (delivery_workers > 1).then(crate::feature::starts_round_begin);
    if let Some(round) = delivery_round {
        let _ = worldgen::with_gen(|g| g.starts_round = Some(round));
    }
    // The terrain rows stop one section below the dimension's own ceiling: a Nether/End
    // window is 0..256 (16 sections), not the Overworld's -64..320.
    let (lo, hi) = (dim.min_y(), dim.min_y() + dim.height() - 16);
    let compression = socket_compression(s);
    let mut failed = false;
    'delivery: for chunk_group in news.chunks(terrain_group(delivery_workers)) {
        prefetch_terrain_group_dim(dim, chunk_group, lo, hi, delivery_workers, delivery_round);
        for (cx, cz) in chunk_group {
            let frame = cached_chunk_frame(compression, gen, dim, *cx, *cz, occluded);
            if write_shared_frame(s, &frame).is_err() {
                failed = true;
                break 'delivery;
            }
            sent.insert((*cx, *cz));
            for n in light_dirty_for_dim(dim, gen, *cx, *cz) {
                dirty.insert(n);
            }
            let snap = signs_in_chunk(dim, *cx, *cz);
            for ((_, sx, sy, sz, front), lines) in snap {
                let _ = send_packet(s, 0x07, sign_entity_pkt(sx, sy, sz, front, &lines));
            }
        }
    }
    if let Some(round) = delivery_round {
        let _ = worldgen::with_gen(|g| g.starts_round = None);
        crate::feature::starts_round_end(round);
    }
    if failed {
        return;
    }
    // A neighbour lit before this batch could not see the emitters generated now, so its light is
    // recomputed and re-sent. `light_dirty_for` limits this to held chunks a new emitter can
    // actually reach, and the set deduplicates the chunks several new neighbours all touch.
    for (cx, cz) in dirty {
        if !sent.contains(&(cx, cz)) {
            continue;
        }
        let __t = chunkprof::enabled().then(std::time::Instant::now);
        let pkt = update_light_packet_lane(
            dim,
            cx,
            cz,
            gen,
            occluded.then(|| underground_occlude_ceil(dim, cx, cz)).flatten(),
        );
        if let Some(t) = __t {
            use std::sync::atomic::Ordering;
            chunkprof::add(&chunkprof::RE_LIGHT, t);
            chunkprof::RE_LIGHT_N.fetch_add(1, Ordering::Relaxed);
        }
        if send_packet(s, 0x2a, pkt).is_err() {
            return;
        }
    }
    let mut bf = Vec::new();
    write_varint(&mut bf, batch as i64);
    if send_packet(s, 0x0c, bf).is_ok() {
        ack.sent_batch(Instant::now(), batch);
    }
}

/// `BuiltInRegistries.BLOCK_ENTITY_TYPE` ids in 1.21.1 for the twelve
/// `live_block_entity_id` families (sign=7 and mob_spawner=9 are already
/// pinned by `sign_entity_pkt` and `generated_spawner_chunk_entities`).
fn live_block_entity_wire_id(id: &str) -> Option<i64> {
    Some(match id {
        "minecraft:jukebox" => 4,
        "minecraft:beacon" => 14,
        "minecraft:skull" => 15,
        "minecraft:banner" => 19,
        "minecraft:lectern" => 29,
        "minecraft:campfire" => 32,
        "minecraft:chiseled_bookshelf" => 38,
        "minecraft:brushable_block" => 39,
        "minecraft:decorated_pot" => 40,
        "minecraft:crafter" => 41,
        "minecraft:trial_spawner" => 42,
        "minecraft:vault" => 43,
        _ => return None,
    })
}

/// The live block entities a 0x27 chunk packet must carry for `(dim, cx, cz)`:
/// each is encoded as `ChunkBlockEntity` (position, type id, network NBT with
/// `x`/`y`/`z`/`id` — the same payload `live_block_entity_snapshot` produces).
fn live_chunk_block_entities(dim: DimId, cx: i32, cz: i32) -> Vec<ChunkBlockEntity> {
    with_save_guard(|| {
        let Some(reg) = crate::block_registry::breg() else {
            return Vec::new();
        };
        let mut store = live_block_entities().lock().unwrap();
        store.discover(reg);
        store
            .entries
            .iter()
            .filter(|(&(edim, x, _, z), _)| edim == dim && x >> 4 == cx && z >> 4 == cz)
            .filter_map(|(&(edim, x, y, z), &(block, ref entity))| {
                // The entry only reaches the client while the block it
                // describes is still in place.
                let state = block_at_dim(edim, x, y, z);
                if reg.block_id_of(state) != Some(block) {
                    return None;
                }
                let id = live_block_entity_id(reg.name_of(state))?;
                let wire_id = live_block_entity_wire_id(id)?;
                let Tag::Compound(mut fields) = entity.to_nbt() else {
                    return None;
                };
                for (field, value) in [("x", x), ("y", y), ("z", z)] {
                    block_entity_put(&mut fields, field, Tag::Int(value));
                }
                block_entity_put(&mut fields, "id", block_entity_string(id));
                let nbt = crate::nbt::write_network_nbt(&Tag::Compound(fields)).ok()?;
                Some(((x, y, z), wire_id, nbt))
            })
            .collect()
    })
}

/// Merge `extra` block-entity entries into an assembled 0x27 payload. The
/// section sits between the chunk section data and the light masks, so it is
/// located by walking the heightmaps compound and the section byte count
/// rather than assuming an offset; a malformed packet is left untouched.
fn splice_chunk_block_entities(packet: &mut Vec<u8>, extra: Vec<ChunkBlockEntity>) {
    if extra.is_empty() {
        return;
    }
    let mut pos = 8usize;
    if packet.len() < pos || !nbt_wire_skip(packet, &mut pos, 0) {
        return;
    }
    // data length varint + the block-states/biomes payload it counts
    {
        let mut rest = &packet[pos..];
        let Some(len) = read_varint(&mut rest) else {
            return;
        };
        pos = packet.len() - rest.len();
        if len < 0 {
            return;
        }
        let Some(end) = pos.checked_add(len as usize) else {
            return;
        };
        if end > packet.len() {
            return;
        }
        pos = end;
    }
    // block-entity count varint, then existing entries
    let count_pos = pos;
    let mut rest = &packet[pos..];
    let Some(existing) = read_varint(&mut rest) else {
        return;
    };
    if !(0..=4096).contains(&existing) {
        return;
    }
    let entries_start = packet.len() - rest.len();
    let mut p = entries_start;
    for _ in 0..existing {
        if p + 3 > packet.len() {
            return;
        }
        p += 3; // packed section-local x/z byte + i16 y
        {
            let mut r = &packet[p..];
            if read_varint(&mut r).is_none() {
                return;
            }
            p = packet.len() - r.len();
        }
        if !nbt_wire_skip(packet, &mut p, 0) {
            return;
        }
    }
    let tail = packet.split_off(p);
    let entries = packet.split_off(entries_start);
    packet.truncate(count_pos);
    write_varint(packet, existing + extra.len() as i64);
    packet.extend_from_slice(&entries);
    for ((x, y, z), wire_id, nbt) in extra {
        packet.push(((x & 15) as u8) << 4 | (z & 15) as u8);
        packet.extend_from_slice(&(y as i16).to_be_bytes());
        write_varint(packet, wire_id);
        packet.extend_from_slice(&nbt);
    }
    packet.extend_from_slice(&tail);
}

/// `minecraft:mob_effect` registry ids for the six effects a beacon offers.
fn beacon_effect_from_wire(id: i64) -> Option<BeaconEffect> {
    Some(match id {
        1 => BeaconEffect::Speed,
        3 => BeaconEffect::Haste,
        5 => BeaconEffect::Strength,
        8 => BeaconEffect::JumpBoost,
        10 => BeaconEffect::Regeneration,
        11 => BeaconEffect::Resistance,
        _ => return None,
    })
}

/// The beacon block a player last used without sneaking, keyed by entity id —
/// the slot a real beacon menu would own. Eids are never reused, so entries
/// are simply dropped with the client's own teardown.
fn beacon_menu_anchors() -> &'static Mutex<HashMap<i32, DimBlockPos>> {
    static ANCHORS: OnceLock<Mutex<HashMap<i32, DimBlockPos>>> = OnceLock::new();
    ANCHORS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The beacon a `set_beacon_effect` refers to. The anchor recorded when the
/// beacon was used wins (a real client sends this packet while its beacon menu
/// is open); without one, the nearest beacon still standing in interaction
/// range stands in for it.
fn live_beacon_anchor(me: &Client, dim: DimId) -> Option<DimBlockPos> {
    let reg = crate::block_registry::breg()?;
    let is_beacon = |x: i32, y: i32, z: i32| {
        live_block_entity_id(reg.name_of(block_at_dim(dim, x, y, z)))
            == Some("minecraft:beacon")
    };
    if let Some(&(adim, x, y, z)) = beacon_menu_anchors().lock().unwrap().get(&me.eid) {
        if adim == dim && is_beacon(x, y, z) {
            return Some((adim, x, y, z));
        }
    }
    let (px, py, pz, ..) = *me.pos.lock().unwrap();
    live_block_entities()
        .lock()
        .unwrap()
        .entries
        .iter()
        .filter(|(&(edim, x, y, z), _)| edim == dim && is_beacon(x, y, z))
        .map(|(&key, _)| {
            let (_, x, y, z) = key;
            (
                (x as f64 + 0.5 - px).powi(2)
                    + (y as f64 + 0.5 - py).powi(2)
                    + (z as f64 + 0.5 - pz).powi(2),
                key,
            )
        })
        .filter(|(d2, _)| *d2 <= 64.0)
        .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(_, key)| key)
}

/// The anti-xray lane a player belongs in right now. Pure: the caller's
/// `currently_revealed` carries the hysteresis.
///
/// * `false` (revealed, real chunks) whenever the feature is off, the
///   dimension has no skylight (Nether/End: open cavities are normal), or the
///   player is a spectator.
/// * A still-occluded player reveals once they are under Y=48, or more than 8
///   blocks under their column's surface — long before the masked ceiling is
///   within the 16 blocks around them.
/// * A revealed player re-occludes only once clearly back on the surface
///   (Y>=60 and within 2 of the local surface), so bobbing across the reveal
///   line never flaps the lane.
fn should_occlude_underground(
    dim: DimId,
    player_y: f64,
    local_surface_y: i32,
    gamemode: u8,
    currently_revealed: bool,
) -> bool {
    if !ac_underground_occlusion_enabled() || !dim.has_skylight() || gamemode == 3 {
        return false;
    }
    if currently_revealed {
        player_y >= 60.0 && player_y >= local_surface_y as f64 - 2.0
    } else {
        !(player_y < 48.0 || player_y < local_surface_y as f64 - 8.0)
    }
}

/// Re-send every chunk the client currently holds in the other lane, nearest
/// first so the reveal reaches the area around the player before anything 16
/// blocks away can be seen. A re-sent `0x27` just replaces the client's copy.
fn resend_loaded_chunks(me: &Client, occluded: bool) {
    let (dim, px, _py, pz, ..) = client_dim_pos(me);
    let (pcx, pcz) = (spatial_chunk_coord(px), spatial_chunk_coord(pz));
    let mut ordered: Vec<(i32, i32)> = me.chunks.lock().unwrap().iter().copied().collect();
    ordered.sort_by_key(|&(cx, cz)| {
        let (dx, dz) = (cx - pcx, cz - pcz);
        dx * dx + dz * dz
    });
    // Lock order matches `pump_client_chunks`: gen_sources before sock.
    let mut gen = me.gen_sources.lock().unwrap();
    let mut sock = me.sock.lock().unwrap();
    let compression = socket_compression(&sock);
    for (cx, cz) in ordered {
        let frame = cached_chunk_frame(compression, &mut gen, dim, cx, cz, occluded);
        if write_shared_frame(&mut sock, &frame).is_err() {
            return;
        }
    }
}

/// Evaluate the player's occlusion lane after a position or game-mode change.
/// Returns the lane to stream in; on a flip it also re-sends every held chunk
/// in the new lane so the transition is seamless both ways (descend: caves and
/// strongholds appear; resurface: anti-cave-xray re-arms).
fn update_underground_lane(me: &Client) -> bool {
    let (dim, px, py, pz, ..) = client_dim_pos(me);
    let gm = *me.gm.lock().unwrap();
    let surface = height_at_dim(dim, px.floor() as i32, pz.floor() as i32);
    let revealed = me.underground_revealed.load(Ordering::Relaxed);
    let occluded = should_occlude_underground(dim, py, surface, gm, revealed);
    if occluded == !revealed {
        return occluded;
    }
    me.underground_revealed.store(!occluded, Ordering::Relaxed);
    resend_loaded_chunks(me, occluded);
    occluded
}

fn pump_client_chunks(client: &Client, announce_center: bool) -> bool {
    let occluded = update_underground_lane(client);
    let mut ack = client.chunk_ack.lock().unwrap();
    let view = *client.view.lock().unwrap();
    let mut chunks = client.chunks.lock().unwrap();
    let mut gen = client.gen_sources.lock().unwrap();
    let mut socket = client.sock.lock().unwrap();
    // Same-dimension teleports also hold the writer while changing position.
    let (dim, x, _, z, yaw, _) = client_dim_pos(client);
    stream_chunks(
        &mut socket,
        &mut chunks,
        &mut gen,
        &mut ack,
        dim,
        ChunkWindow::at(x, z, view),
        yaw,
        announce_center,
        None,
        occluded,
    );
    !ack.awaiting()
        && with_play_wire(&socket, |play| play.chunk_delivery.has_pending()).unwrap_or(false)
}

/// Skip one **unnamed-root** network NBT value (the shape carried inside a
/// `minecraft:dimension_type` registry entry) starting at `*p`. Returns false on any
/// truncation or absurd length, so a corrupt `config_packets.bin` fails closed rather than
/// handing out a wrong dimension id.
fn nbt_wire_skip(data: &[u8], p: &mut usize, depth: usize) -> bool {
    if depth > 32 || *p >= data.len() {
        return false;
    }
    let tag = data[*p];
    *p += 1;
    nbt_wire_value(data, p, tag, depth)
}
/// Skip the *payload* of a value whose tag byte has already been consumed.
fn nbt_wire_value(data: &[u8], p: &mut usize, tag: u8, depth: usize) -> bool {
    if depth > 32 || *p > data.len() {
        return false;
    }
    fn take(data: &[u8], p: &mut usize, n: usize) -> bool {
        n <= data.len().saturating_sub(*p) && {
            *p += n;
            true
        }
    }
    fn num(data: &[u8], p: &mut usize, n: usize) -> Option<u64> {
        if !take(data, p, n) {
            return None;
        }
        Some(data[*p - n..*p].iter().fold(0u64, |v, &b| (v << 8) | u64::from(b)))
    }
    match tag {
        0 => true,
        1 => take(data, p, 1),
        2 => take(data, p, 2),
        3 => take(data, p, 4),
        4 => take(data, p, 8),
        5 => take(data, p, 4),
        6 => take(data, p, 8),
        7 | 11 | 12 => {
            let width = if tag == 7 { 1 } else if tag == 11 { 4 } else { 8 };
            let Some(n) = num(data, p, 4) else { return false };
            n <= (1 << 24) && take(data, p, (n as usize).saturating_mul(width))
        }
        8 => {
            let Some(n) = num(data, p, 2) else { return false };
            take(data, p, n as usize)
        }
        9 => {
            let Some(elem) = data.get(*p).copied() else { return false };
            *p += 1;
            let Some(n) = num(data, p, 4) else { return false };
            if n > (1 << 20) {
                return false;
            }
            for _ in 0..n {
                if !nbt_wire_value(data, p, elem, depth + 1) {
                    return false;
                }
            }
            true
        }
        10 => loop {
            let Some(child) = data.get(*p).copied() else { return false };
            *p += 1;
            if child == 0 {
                return true;
            }
            let Some(n) = num(data, p, 2) else { return false };
            if !take(data, p, n as usize) || !nbt_wire_value(data, p, child, depth + 1) {
                return false;
            }
        },
        _ => false,
    }
}

/// The `dimension_type` registry ids from `config_packets.bin`, indexed by `DimId::index()`.
///
/// The registry order is a property of the pinned blob, not of this port's slot order:
/// `Overworld = 0`, `End = 2`, `Nether = 3` (slot 1 is `overworld_caves`, which the server
/// never sends a player to). Reading the blob keeps the wire id and the 0x2b/0x47 payload in
/// agreement if the blob is ever regenerated with a different order.
fn parse_dim_type_wire_ids() -> Option<[i32; 3]> {
    for (id, packet) in captured_config_packets()? {
        if id != 0x07 {
            continue;
        }
        let (name, body) = config_registry_header(&packet)?;
        if name != "minecraft:dimension_type" {
            continue;
        }
        return dim_wire_ids_from_entries(registry_wire_entries(body)?);
    }
    None
}

/// Map `dimension_type` registry entry names to their wire ids, indexed by
/// `DimId::index()`. A datapack dimension can register a slot >= 3 whose
/// canonical id also appears as a registry entry name; the pinned table only
/// has room for the three vanilla ids, so out-of-range slots are skipped
/// rather than indexing past the array.
fn dim_wire_ids_from_entries(entries: Vec<(&str, &[u8])>) -> Option<[i32; 3]> {
    let mut ids = [-1i32; 3];
    for (index, (entry, _)) in entries.into_iter().enumerate() {
        if let Some(dim) = DimId::parse_dimension_id(entry) {
            if dim.index() < ids.len() {
                ids[dim.index()] = index as i32;
            }
        }
    }
    ids.iter().all(|&id| id >= 0).then_some(ids)
}

static DIM_WIRE_IDS: OnceLock<Option<[i32; 3]>> = OnceLock::new();
/// Fallback for a server started without a readable `config_packets.bin`: the ids in the
/// pinned blob this crate ships. The unit test asserts these are the values actually parsed.
const DIM_WIRE_IDS_FALLBACK: [i32; 3] = [0, 3, 2];
fn dim_type_wire_ids() -> [i32; 3] {
    *DIM_WIRE_IDS
        .get_or_init(parse_dim_type_wire_ids)
        .as_ref()
        .unwrap_or(&DIM_WIRE_IDS_FALLBACK)
}
fn dim_type_wire_id(dim: DimId) -> i32 {
    if dim.is_vanilla() {
        return dim_type_wire_ids()[dim.index()];
    }
    // A custom dimension answers the index its `dimension_type` holds in the merged
    // registry — where its datapack's type file landed or where
    // `append_custom_dimension_types` appended it — never a slot number.
    let Some(spec) = worldgen::dimension_spec(dim) else {
        return dim_type_wire_ids()[0];
    };
    dim_type_entry_ids()
        .lock()
        .unwrap()
        .get(spec.type_key)
        .copied()
        .unwrap_or_else(|| dim_type_wire_ids()[0])
}

/// 0x2b Join Game. Every dimension type the client can be moved into is declared so a
/// later 0x47 Respawn can name one the client already knows; the spawn info carries the
/// dimension the player actually starts in.
fn join_game(dim: DimId, gm: u8, eid: i32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&eid.to_be_bytes());
    p.push(0);
    let dimensions = DimId::all_registered();
    write_varint(&mut p, dimensions.len() as i64);
    for d in dimensions {
        write_str(&mut p, d.dimension_id());
    }
    write_varint(&mut p, 20);
    write_varint(&mut p, SERVER_VIEW_DISTANCE as i64);
    write_varint(&mut p, 8);
    p.push(0);
    p.push(1);
    p.push(0);
    p.extend_from_slice(&spawn_info_dim(dim, gm));
    p.push(0); // secure chat off
    p
}

/// The spawn-info sub-struct shared by 0x2b Join Game and 0x47 Respawn. `dim_type` is the
/// registry id from `config_packets.bin` (NOT `DimId::index()`), and the name is the
/// dimension's resource id.
fn spawn_info_dim(dim: DimId, gm: u8) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, dim_type_wire_id(dim) as i64);
    write_str(&mut p, dim.dimension_id());
    p.extend_from_slice(&0i64.to_be_bytes());
    p.push(gm);
    p.push(255); // prev -1
    p.push(0);
    p.push(1); // isDebug=false, isFlat=true
    p.push(0); // no death loc
    write_varint(&mut p, 0);
    p
}
/// Move one connected player to another dimension and rebuild the authoritative window.
///
/// The 0x47 Respawn packet names the destination through `spawn_info_dim`, so the client
/// re-interprets every following 0x27 chunk with the destination's section count and sky
/// light rules. The chunk set and the light sources are dimension-local, so they are only
/// dropped when the dimension actually changes; an End return to a different Overworld
/// position in the same dimension keeps the window and just streams the new centre.
fn switch_client_dimension(
    me: &Arc<Client>,
    new_dim: DimId,
    target: (f64, f64, f64, f32, f32),
    view: i32,
) {
    let old_dim = *me.dim.lock().unwrap();
    close_live_menu(me, true, true);
    let had_furnace = me.open_furnace.lock().unwrap().take().is_some();
    if had_furnace {
        let _ = send_packet(&mut me.sock.lock().unwrap(), 0x12, vec![2]);
    }
    // Retire everything the old world was tracking before the client drops its chunks.
    let mut seen = me.seen.lock().unwrap();
    // Serialize the transition with the connection thread's chunk pump.
    let mut ack = me.chunk_ack.lock().unwrap();
    if !seen.is_empty() {
        let removed: Vec<_> = seen.drain().collect();
        let _ = send_packet(
            &mut me.sock.lock().unwrap(),
            0x42,
            remove_entities_payload(&removed),
        );
    }
    if old_dim != new_dim {
        me.chunks.lock().unwrap().clear();
        me.gen_sources.lock().unwrap().clear();
    }
    let teleport_id = {
        let mut teleport = me.teleport.lock().unwrap();
        let id = teleport.begin();
        ac_expect_teleport(me.eid);
        // Spawn eligibility reads this pair in the same lock order.
        let mut dim = me.dim.lock().unwrap();
        let mut p = me.pos.lock().unwrap();
        *dim = new_dim;
        *p = (target.0, target.1, target.2, target.3, target.4, true);
        id
    };
    drop(seen);
    *me.in_portal.lock().unwrap() = false;
    *me.fall_from.lock().unwrap() = target.1;
    *ack = connection_limits::chunk_feedback(window_batch(view));
    let gm = *me.gm.lock().unwrap();
    let mut rp = spawn_info_dim(new_dim, gm);
    rp.push(0x03); // data_kept: keep inventory and attributes across the move
    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x47, rp);
    let mut ge = vec![13u8]; // START_WAITING_CHUNKS
    ge.extend_from_slice(&0f32.to_be_bytes());
    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x22, ge);
    // A dimension hop re-derives the lane from scratch: Nether/End always get
    // the real lane, and the hysteresis restarts on the new surface.
    let dim_occluded = update_underground_lane(me);
    {
        let mut ch = me.chunks.lock().unwrap();
        let mut gen = me.gen_sources.lock().unwrap();
        stream_chunks(
            &mut me.sock.lock().unwrap(),
            &mut ch,
            &mut gen,
            &mut ack,
            new_dim,
            ChunkWindow::at(target.0, target.2, view),
            target.3,
            true,
            None,
            dim_occluded,
        );
    }
    let mut sock = me.sock.lock().unwrap();
    let mut teleport = me.teleport.lock().unwrap();
    if teleport.preparing(teleport_id) {
        send_prepared_teleport(&mut sock, &mut teleport, target, teleport_id);
    }
}

/// The portal block states this port recognises (`assets/blocks_1211.json`). The nether
/// portal has two axis states; the end portal has one.
fn is_nether_portal_state(st: u32) -> bool {
    (5864..=5865).contains(&st)
}
fn is_end_portal_state(st: u32) -> bool {
    st == 7406
}

/// Materialise a safe landing in a Nether or Overworld destination: an obsidian floor under
/// the teleported column, clear space above it, and a complete obsidian portal frame with a
/// live portal inside. Materialising the frame matters for the bounce guard — the arrival
/// portal is intentional, and a per-player flag (not the block's absence) stops the return.
fn materialize_nether_landing(dim: DimId, bx: i32, by: i32, bz: i32) {
    let obsidian = name_to_state("obsidian").unwrap_or(2354);
    for dx in -2..=2 {
        for dz in -2..=2 {
            set_block_dim(dim, bx + dx, by - 1, bz + dz, obsidian);
            for dy in by..by + 4 {
                set_block_dim(dim, bx + dx, dy, bz + dz, 0);
            }
        }
    }
    for dy in 0..4 {
        set_block_dim(dim, bx - 1, by + dy, bz, obsidian);
        set_block_dim(dim, bx + 1, by + dy, bz, obsidian);
    }
    set_block_dim(dim, bx, by - 1, bz, obsidian);
    set_block_dim(dim, bx, by + 3, bz, obsidian);
    for dy in 0..3 {
        set_block_dim(dim, bx, by + dy, bz, 5864);
    }
}

/// The End spawn platform: vanilla's obsidian 5x5 at (100, 48, 0) with three air layers
/// above it. Portalled players always arrive here, so it must exist even in a fresh world.
fn materialize_end_spawn() {
    let obsidian = name_to_state("obsidian").unwrap_or(2354);
    for dx in -2..=2 {
        for dz in -2..=2 {
            set_block_dim(DimId::End, 100 + dx, 48, dz, obsidian);
            for y in 49..=51 {
                set_block_dim(DimId::End, 100 + dx, y, dz, 0);
            }
        }
    }
}

/// The Nether portal landing, as a pure function of the source block: 1:8 horizontal
/// scaling and a y clamped into the destination's own 0..256 window. Split out so the
/// scaling and the bounce guard can be asserted without a live connection.
fn nether_portal_target(dim: DimId, bx: i32, by: i32, bz: i32) -> Option<(DimId, i32, i32, i32)> {
    if dim == DimId::End {
        return None;
    }
    let (ndim, nx, nz) = if dim == DimId::Overworld {
        (DimId::Nether, bx as f64 / 8.0, bz as f64 / 8.0)
    } else {
        (DimId::Overworld, bx as f64 * 8.0, bz as f64 * 8.0)
    };
    let ny = (by as f64).clamp(
        (ndim.min_y() + 4) as f64,
        (ndim.min_y() + ndim.height() - 8) as f64,
    );
    Some((ndim, nx.floor() as i32, ny.floor() as i32, nz.floor() as i32))
}

/// Complete one portal trip. `st` is the block the player stood in, which decides whether
/// this is an Overworld<->Nether trip (1:8 horizontal scaling) or an End trip.
fn enter_portal(
    me: &Arc<Client>,
    st: u32,
    bx: i32,
    by: i32,
    bz: i32,
    yaw: f32,
    pitch: f32,
    view: i32,
) {
    let dim = *me.dim.lock().unwrap();
    if is_end_portal_state(st) {
        if dim == DimId::Overworld {
            materialize_end_spawn();
            switch_client_dimension(me, DimId::End, (100.5, 49.0, 0.5, yaw, pitch), view);
        } else {
            let (sdim, sx, sy, sz) = {
                let sdim = *me.spawn_dim.lock().unwrap();
                let pos = me.spawn_pt.lock().unwrap().unwrap_or_else(|| {
                    let w = *WORLD_SPAWN.lock().unwrap();
                    (w.0, w.1, w.2, w.3)
                });
                (sdim, pos.0, pos.1, pos.2)
            };
            switch_client_dimension(me, sdim, (sx, sy, sz, yaw, pitch), view);
        }
    } else {
        // 1:8 horizontal scaling. Overworld (x,z) -> Nether (x/8, z/8) and back.
        let (ndim, nx, ny, nz) =
            nether_portal_target(dim, bx, by, bz).expect("only the End has no Nether portal");
        materialize_nether_landing(ndim, nx, ny, nz);
        switch_client_dimension(
            me,
            ndim,
            (nx as f64 + 0.5, ny as f64, nz as f64 + 0.5, yaw, pitch),
            view,
        );
    }
    // Arrival is inside a real portal; hold the per-player guard until the player steps out
    // so the destination portal cannot send them straight back.
    *me.in_portal.lock().unwrap() = true;
}

/// Called after a movement frame. Returns whether a dimension transition happened.
fn tick_portal(me: &Arc<Client>, view: i32) -> bool {
    let dim = *me.dim.lock().unwrap();
    let (px, py, pz, yaw, pitch, _) = *me.pos.lock().unwrap();
    let (bx, by, bz) = (px.floor() as i32, py.floor() as i32, pz.floor() as i32);
    let st = block_at_dim(dim, bx, by, bz);
    if !is_nether_portal_state(st) && !is_end_portal_state(st) {
        *me.in_portal.lock().unwrap() = false;
        *me.portal_at.lock().unwrap() = None;
        return false;
    }
    if *me.in_portal.lock().unwrap() {
        return false; // arrival guard: wait until the player leaves the portal block
    }
    *me.in_portal.lock().unwrap() = true;
    let creative = *me.gm.lock().unwrap_or_else(|e| e.into_inner()) != 0;
    // `playersNetherPortalDefaultDelay` (survival, 80 GT = 4 s) and
    // `playersNetherPortalCreativeDelay` (creative/spectator, 1 GT ≈ instant)
    // are vanilla game ticks of 50 ms; a delay of 0 disables the wait.
    let delay_gt = gamerule_live_value(if creative {
        "playersNetherPortalCreativeDelay"
    } else {
        "playersNetherPortalDefaultDelay"
    })
    .max(0) as u64;
    if delay_gt == 0 || (creative && delay_gt <= 1) {
        enter_portal(me, st, bx, by, bz, yaw, pitch, view);
        return true;
    }
    let mut at = me.portal_at.lock().unwrap();
    if at.is_none() {
        *at = Some(Instant::now());
        return false;
    }
    if at.unwrap().elapsed() < Duration::from_millis(50 * delay_gt) {
        return false;
    }
    *at = None;
    drop(at);
    enter_portal(me, st, bx, by, bz, yaw, pitch, view);
    true
}

static SIGNS: Registry<SignKey, SignLines> = OnceLock::new();
fn signs() -> &'static LockedMap<SignKey, SignLines> {
    SIGNS.get_or_init(|| Mutex::new(HashMap::new()))
}
/// The signs one dimension's chunk packet carries. One filter owns the dimension check so a chunk
/// sent to the Nether cannot answer with an Overworld sign at the same coordinates (and vice versa).
fn signs_in_chunk(dim: DimId, cx: i32, cz: i32) -> Vec<(SignKey, SignLines)> {
    signs()
        .lock()
        .unwrap()
        .iter()
        .filter(|(&(sdim, x, _, z, _), _)| sdim == dim && x >> 4 == cx && z >> 4 == cz)
        .map(|(k, v)| (*k, v.clone()))
        .collect()
}
fn is_sign_block_state(state: u32) -> bool {
    crate::block_registry::breg().is_some_and(|reg| {
        let name = reg.canonical_state(state);
        let block = name.split_once('[').map_or(name.as_str(), |(block, _)| block);
        block.strip_prefix("minecraft:").is_some_and(|name| {
            name.ends_with("_sign") || name.ends_with("_wall_sign")
                || name.ends_with("_hanging_sign") || name.ends_with("_wall_hanging_sign")
        })
    })
}
fn sign_edit_in_range(pos: (f64, f64, f64), x: i32, y: i32, z: i32) -> bool {
    let (dx, dy, dz) = (
        pos.0 - (x as f64 + 0.5),
        pos.1 - (y as f64 + 0.5),
        pos.2 - (z as f64 + 0.5),
    );
    dx * dx + dy * dy + dz * dz <= 64.0
}
fn nbt_named_str(v: &mut Vec<u8>, name: &str, val: &str) {
    v.push(8);
    nbt_str(v, name);
    nbt_str(v, val);
}
fn nbt_named_byte(v: &mut Vec<u8>, name: &str, b: u8) {
    v.push(1);
    nbt_str(v, name);
    v.push(b);
}
/// A plain string component, as the game encodes it: `"Hi"`, not `{"text":"Hi"}`.
fn json_text(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}
/// A sign side, in the shape the game itself writes.
///
/// Measured from native (`SignText.DIRECT_CODEC` encoded with the level's ops, see
/// `~/.cache/mc-spine-audit/save-booking-20261110/sign-probe6.log`):
/// `{color, filtered_messages:[4], has_glowing_text, messages:[4]}` with each message a
/// **JSON string** component (`'"Hi"'`), not a `{"text":...}` object, and with
/// `filtered_messages` present. A tag without that list loads as empty text natively, so
/// writing the shorter shape here would make a client show blank lines.
fn sign_side_nbt(v: &mut Vec<u8>, name: &str, lines: &[String; 4]) {
    v.push(10);
    nbt_str(v, name); // TAG_Compound <name>
    nbt_named_str(v, "color", "white");
    nbt_named_byte(v, "has_glowing_text", 0);
    for list in ["filtered_messages", "messages"] {
        v.push(9);
        nbt_str(v, list); // TAG_List<String>
        v.push(8);
        v.extend_from_slice(&4i32.to_be_bytes());
        for l in lines {
            let text = if list == "messages" { json_text(l) } else { String::new() };
            nbt_str(v, &text);
        }
    }
    v.push(0);
}
fn sign_entity_pkt(x: i32, y: i32, z: i32, front: u8, lines: &[String; 4]) -> Vec<u8> {
    // 0x07 tile_entity_data
    let empty: [String; 4] = [String::new(), String::new(), String::new(), String::new()];
    let (fl, bl) = if front != 0 {
        (lines, &empty)
    } else {
        (&empty, lines)
    };
    let mut p = Vec::new();
    write_pos(&mut p, x, y, z);
    write_varint(&mut p, 7); // block entity type: sign
    p.push(10); // root compound (anonymousNbt)
    sign_side_nbt(&mut p, "front_text", fl);
    sign_side_nbt(&mut p, "back_text", bl);
    nbt_named_byte(&mut p, "is_waxed", 0);
    p.push(0);
    p
}
fn sign_base_for_item(item: i64) -> Option<u32> {
    Some(match item {
        886 => 4302,
        887 => 4334,
        888 => 4366,
        889 => 4462,
        890 => 4398,
        891 => 4430,
        892 => 4494,
        893 => 4526,
        894 => 4558,
        895 => 19276,
        896 => 19308,
        _ => return None,
    })
}
fn chat_nbt(text: &str) -> Vec<u8> {
    // {"text":"..."} as anonymousNbt compound
    let mut v = vec![10u8];
    v.push(8); // TAG_String
    nbt_str(&mut v, "text");
    nbt_str(&mut v, text);
    v.push(0);
    v
}
fn teleport_in_dimension(
    c: &Client,
    sock: &mut TcpStream,
    (x, y, z, yaw, pitch): (f64, f64, f64, f32, f32),
) {
    let mut teleport = c.teleport.lock().unwrap();
    let id = teleport.begin();
    ac_expect_teleport(c.eid);
    *c.pos.lock().unwrap() = (x, y, z, yaw, pitch, true);
    send_prepared_teleport(sock, &mut teleport, (x, y, z, yaw, pitch), id);
}

fn send_prepared_teleport(
    sock: &mut TcpStream,
    teleport: &mut connection_limits::TeleportSync,
    (x, y, z, yaw, pitch): (f64, f64, f64, f32, f32),
    id: i32,
) {
    if send_packet(sock, 0x40, teleport_pkt(x, y, z, yaw, pitch, i64::from(id))).is_ok() {
        teleport.sent();
    }
}

fn teleport_pkt(x: f64, y: f64, z: f64, yaw: f32, pitch: f32, tid: i64) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&x.to_be_bytes());
    p.extend_from_slice(&y.to_be_bytes());
    p.extend_from_slice(&z.to_be_bytes());
    p.extend_from_slice(&yaw.to_be_bytes());
    p.extend_from_slice(&pitch.to_be_bytes());
    p.push(0);
    write_varint(&mut p, tid);
    p
}

fn spawn_position_packet((x, y, z, yaw): (f64, f64, f64, f32)) -> Vec<u8> {
    let mut payload = Vec::with_capacity(12);
    write_pos(
        &mut payload,
        x.floor() as i32,
        y.floor() as i32,
        z.floor() as i32,
    );
    payload.extend_from_slice(&yaw.to_be_bytes());
    payload
}

#[cfg(feature = "neoforge-events")]
fn neoforge_event(
    source: &Client,
    event: java_compat::S1Event<String>,
) -> Result<Option<String>, String> {
    let Some(runtime) = java_compat::NEOFORGE.get() else {
        return Ok(None);
    };
    let mut outcome = runtime.event(event).inspect_err(|error| {
        eprintln!("[mod-compatibility] NeoForge S1 callback: {error}");
    })?;
    if outcome.canceled {
        return Err("canceled".into());
    }
    let chat = outcome.chat_message.take();
    let report = java_compat::apply_s1_outcome(outcome).inspect_err(|error| {
        eprintln!("[mod-compatibility] NeoForge S1 outcome: {error}");
    })?;
    deliver_neoforge_messages(Some(source), report.system_messages);
    Ok(chat)
}

#[cfg(feature = "neoforge-events")]
fn deliver_neoforge_messages(source: Option<&Client>, messages: Vec<(i32, String)>) {
    if !messages.is_empty() {
        let audience = client_snapshot();
        for (eid, text) in messages {
            let recipient = source.filter(|client| client.eid == eid).or_else(|| {
                audience
                    .iter()
                    .find(|client| client.eid == eid)
                    .map(Arc::as_ref)
            });
            if let Some(client) = recipient {
                let _ = send_packet(&mut client.sock.lock().unwrap(), 0x6c, system_chat(&text));
            }
        }
    }
}

#[cfg(feature = "neoforge-events")]
fn neoforge_player_tick() {
    use landing_types::{S3Activation, S3TickRequest, S3_MAX_RECORDS};

    let Some(runtime) = java_compat::NEOFORGE.get() else {
        return;
    };
    let requests: Vec<_> = client_snapshot()
        .into_iter()
        .filter_map(|client| {
            let (dim, x, y, z, ..) = client_dim_pos(&client);
            if ![x, y, z].into_iter().all(f64::is_finite) {
                return None;
            }
            let pos = (x.floor() as i32, y.floor() as i32, z.floor() as i32);
            check_modification_bounds(dim, pos).ok()?;
            Some(S3TickRequest {
                activation: S3Activation::EntityTick,
                // This activation uses protocol 767 entity type IDs as kinds.
                kind: PLAYER_ENTITY_TYPE as u16,
                dim: dim.index() as u8,
                id: client.eid,
                // EIDs are allocated for the connection lifetime.
                generation: u32::try_from(client.eid).ok()?,
                pos,
                state: block_at_dim(dim, pos.0, pos.1, pos.2),
            })
        })
        .collect();
    for chunk in requests.chunks(S3_MAX_RECORDS) {
        match runtime
            .player_ticks(chunk.to_vec())
            .and_then(java_compat::apply_s1_outcome)
        {
            Ok(report) => deliver_neoforge_messages(None, report.system_messages),
            Err(error) => eprintln!("[mod-compatibility] NeoForge player tick: {error}"),
        }
    }
}

#[cfg(feature = "neoforge-events")]
fn neoforge_resync_block(client: &Client, dim: DimId, (x, y, z): BlockPos) {
    let state = block_at_dim(dim, x, y, z);
    let visible = visible_block_update_state(dim, x, y, z, state);
    let mut body = Vec::new();
    write_pos(&mut body, x, y, z);
    write_varint(&mut body, i64::from(visible));
    let _ = send_packet(&mut client.sock.lock().unwrap(), 0x09, body);
}

#[cfg(feature = "neoforge-events")]
fn neoforge_place_allowed(
    client: &Client,
    (dim, x, y, z): DimBlockPos,
    replaced: u32,
    placed: u32,
    against: u32,
) -> bool {
    if java_compat::NEOFORGE.get().is_none() {
        return true;
    }
    if neoforge_event(
        client,
        java_compat::S1Event::BlockPlace {
            dim: dim.index() as u8,
            pos: (x, y, z),
            replaced,
            placed,
            against,
            player: (client.eid, client.name.clone()),
        },
    )
    .is_err()
    {
        neoforge_resync_block(client, dim, (x, y, z));
        return false;
    }
    true
}

// 1.21.1 GameProtocols registers Custom Payload at 0x12 C2S and 0x19 S2C.
#[cfg(feature = "neoforge-events")]
fn neoforge_payload(client: &Client, mut body: &[u8]) {
    use landing_types::{CustomPayloadEnvelope, S4_MAX_CHANNEL, S4_MAX_PAYLOAD};
    let Some(runtime) = java_compat::NEOFORGE.get() else {
        return;
    };
    let Some(len) = read_protocol_i32(&mut body).and_then(|n| usize::try_from(n).ok()) else {
        return;
    };
    if len > S4_MAX_CHANNEL || len > body.len() || body.len() - len > S4_MAX_PAYLOAD {
        return;
    }
    let (channel, bytes) = body.split_at(len);
    let Ok(channel) = std::str::from_utf8(channel) else {
        return;
    };
    if channel.starts_with("minecraft:") {
        return;
    }
    let Ok(outbound) = runtime.payload(
        client.eid,
        client.name.clone(),
        CustomPayloadEnvelope {
            channel: channel.into(),
            bytes: bytes.to_vec(),
            target_eid: None,
        },
    ) else {
        return;
    };
    if outbound.is_empty() {
        return;
    }
    let audience = client_snapshot();
    for envelope in outbound {
        let mut packet = Vec::new();
        write_str(&mut packet, &envelope.channel);
        packet.extend_from_slice(&envelope.bytes);
        for recipient in &audience {
            if envelope.target_eid.is_none_or(|eid| eid == recipient.eid) {
                let _ = send_packet(&mut recipient.sock.lock().unwrap(), 0x19, packet.clone());
            }
        }
    }
}

fn system_chat(text: &str) -> Vec<u8> {
    let mut p = chat_nbt(text);
    p.push(0);
    p
}
// tellraw: build anonymousNbt compound from a flat json component
// (scalar fields only — nested arrays/components documented gap)
fn json_component_nbt(json: &str) -> Vec<u8> {
    let mut v = vec![10u8];
    for (k, is_bool) in [
        ("text", false),
        ("color", false),
        ("font", false),
        ("insertion", false),
        ("bold", true),
        ("italic", true),
        ("underlined", true),
        ("strikethrough", true),
        ("obfuscated", true),
    ] {
        if let Some(val) = jstr(json, k) {
            if is_bool {
                v.push(1);
                nbt_str(&mut v, k);
                v.push(if val == "true" { 1 } else { 0 });
            } else {
                v.push(8);
                nbt_str(&mut v, k);
                nbt_str(&mut v, val);
            }
        }
    }
    if v.len() == 1 {
        v.push(8);
        nbt_str(&mut v, "text");
        nbt_str(&mut v, json);
    }
    v.push(0);
    v
}
fn ack(seq: i64) -> Vec<u8> {
    let mut v = Vec::new();
    write_varint(&mut v, seq);
    v
}
const FACE_OFF: [(i32, i32, i32); 6] = [
    (0, -1, 0),
    (0, 1, 0),
    (0, 0, -1),
    (0, 0, 1),
    (-1, 0, 0),
    (1, 0, 0),
];

// Decode protocol-767 handshake without trusting unbounded client lengths.
fn handshake_parts(hs: &[u8]) -> Option<(i64, i64)> {
    let mut p = hs;
    let protocol = read_varint(&mut p)?;
    let address_len = usize::try_from(read_varint(&mut p)?).ok()?;
    if address_len > 255 || p.len() < address_len.checked_add(2)? {
        return None;
    }
    std::str::from_utf8(&p[..address_len]).ok()?;
    p = &p[address_len + 2..]; // address + two-byte port
    let next = read_varint(&mut p)?;
    matches!(next, 1 | 2).then_some((protocol, next))
}

// Protocol 767 includes a UUID in Login Start. In offline mode the server
// derives the UUID from the validated name, as Java nameUUIDFromBytes does.
fn login_identity(body: &[u8]) -> Option<(String, [u8; 16])> {
    let mut p = body;
    let name_len = usize::try_from(read_varint(&mut p)?).ok()?;
    if name_len > 16 || p.len() < name_len + 16 {
        return None;
    }
    let name = std::str::from_utf8(&p[..name_len]).ok()?;
    if !valid_player_name(name) {
        return None;
    }
    let material = format!("OfflinePlayer:{name}");
    let mut uuid = generation_math::md5(material.as_bytes());
    uuid[6] = (uuid[6] & 0x0f) | 0x30;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    Some((name.to_owned(), uuid))
}

/// Login-state Disconnect (0x00): the client shows `reason` on its disconnect screen.
/// `reason` must be a plain string without quotes or backslashes (all callers pass literals).
fn login_disconnect(s: &mut TcpStream, reason: &str) {
    let mut pl = Vec::new();
    write_str(&mut pl, &format!("{{\"text\":\"{reason}\"}}"));
    let _ = send_packet(s, 0, pl);
}
/// Play-state Disconnect (0x1d); same delivery as the other clientbound packets.
fn play_disconnect(s: &mut TcpStream, reason: &str) {
    let _ = send_packet(s, 0x1d, chat_nbt(reason));
}

fn disconnect_all_clients(reason: &str) {
    for client in client_snapshot() {
        let mut socket = client.sock.lock().unwrap_or_else(|error| error.into_inner());
        play_disconnect(&mut socket, reason);
        let _ = socket.shutdown(std::net::Shutdown::Both);
    }
}
/// One line per refused connection, at most one per second so a flood cannot
/// turn the log itself into the denial of service.
fn log_refusal(ip: &str, why: &str) {
    static START: OnceLock<Instant> = OnceLock::new();
    static LAST: AtomicU64 = AtomicU64::new(u64::MAX);
    let now = START.get_or_init(Instant::now).elapsed().as_secs();
    if LAST.swap(now, Ordering::Relaxed) != now {
        eprintln!("[security] refused connection from {ip}: {why}");
    }
}

// The owning connection checks for deaths even when no play packets arrive.
// PvP invokes this on the victim as well, so drops happen at the death site.
fn death_drop_marks() -> &'static Mutex<HashSet<i32>> {
    static MARKS: OnceLock<Mutex<HashSet<i32>>> = OnceLock::new();
    MARKS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn drop_player_items_on_death(player: &Arc<Client>) {
    if *player.hp.lock().unwrap() > 0.0 {
        return;
    }
    if !death_drop_marks().lock().unwrap().insert(player.eid) {
        return;
    }
    // vanilla LivingEntity.die → broadcastEntityEvent(entity, (byte) 3): the
    // death event goes to the victim and every observer that tracks them,
    // regardless of keepInventory or gamemode.
    let death_evt = entity_event_pkt(player.eid, 3);
    if let Ok(mut w) = player.sock.lock() {
        let _ = send_packet(&mut w, 0x1f, death_evt.clone());
        // Game Event (0x22) id 11 = IMMEDIATE_RESPAWN: 1.0 tells the client to
        // respawn without the death screen — vanilla's doImmediateRespawn.
        let mut ge = vec![11u8];
        ge.extend_from_slice(
            &((GR_DO_IMMEDIATE_RESPAWN.load(Ordering::Relaxed) != 0) as i32 as f32)
                .to_be_bytes(),
        );
        let _ = send_packet(&mut w, 0x22, ge);
    }
    broadcast_seen_ex(player.eid, player.eid, |_c| (0x1f, death_evt.clone()));
    if GR_KEEP_INVENTORY.load(Ordering::Relaxed) != 0
        || !matches!(*player.gm.lock().unwrap(), 0 | 2)
    {
        return;
    }
    close_live_menu(player, true, true);
    let (dx, dy, dz, ..) = *player.pos.lock().unwrap();
    let mut spill: Vec<Stack> = {
        let mut inventory = player.inv.lock().unwrap();
        let items = inventory.values().cloned().collect();
        inventory.clear();
        items
    };
    *player.armor_pts.lock().unwrap() = 0;
    let cursor = std::mem::take(&mut *player.cursor.lock().unwrap());
    if cursor.1 > 0 {
        spill.push(cursor);
    }
    for grid in [&player.craft, &player.craft9] {
        spill.extend(std::mem::take(&mut *grid.lock().unwrap()).into_values());
    }
    {
        let mut spawned = drops().lock().unwrap();
        for (item, cnt, comps) in spill
            .into_iter()
            .filter(|stack| stack.1 > 0 && enchant_level(&stack.2, 40) == 0)
        {
            spawned.push(Drop {
                eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                x: dx,
                y: dy + 0.4,
                z: dz,
                item,
                cnt,
                comps,
                age: 0,
                target: None,
                thrower: None,
            });
        }
    }
    let mut experience = take_dropped_experience(player);
    let mut spawned = xps().lock().unwrap();
    while experience > 0 {
        let count = experience.min(10) as i16;
        experience -= i64::from(count);
        spawned.push(Xp {
            eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
            x: dx,
            y: dy + 0.5,
            z: dz,
            val: count,
            age: 0,
        });
    }
}

fn save_live_player_snapshot(
    player: &Arc<Client>,
    position: DimPosRot,
    inventory: &HashMap<i16, Stack>,
) {
    let mut snapshot = PlayerSave::capture(player, position, inventory);
    let mut saved = saved_players().lock().unwrap();
    if let (Some(old), Some(current)) = (
        saved.get(&player.uuid).and_then(|save| save.respawn),
        snapshot.respawn,
    ) {
        if old.dim == current.dim
            && old.position == current.position
            && old.angle == current.angle
        {
            snapshot.respawn = Some(old);
        }
    }
    saved.insert(player.uuid, snapshot);
}

// ---------- W3 live protocol wiring ----------

/// The live copies of the Vanilla ops/ban/whitelist files, loaded lazily from
/// the server working directory on the first login and refreshed by the admin
/// commands. `None` means "not loaded yet"; `max_players` snapshots
/// `MC_SPINE_MAX_PLAYERS` (default 20) at load so the check is a single read.
struct LiveAdmission {
    ops: OpsRegistry,
    bans: BanRegistry,
    whitelist: WhitelistRegistry,
    max_players: usize,
}

impl LiveAdmission {
    fn load() -> Self {
        let dir = std::path::Path::new(".");
        let mut whitelist =
            WhitelistRegistry::load(&dir.join(WHITELIST_FILE)).unwrap_or_default();
        // `enforceWhitelist` is a server.properties fact upstream; mc-spine keeps
        // it in the environment so whitelist.json stays data-only.
        whitelist.enabled = std::env::var("MC_SPINE_WHITELIST")
            .map(|v| matches!(v.as_str(), "1" | "true" | "on"))
            .unwrap_or(false);
        Self {
            ops: OpsRegistry::load(&dir.join(OPS_FILE)).unwrap_or_default(),
            bans: BanRegistry::load(dir).unwrap_or_default(),
            whitelist,
            max_players: std::env::var("MC_SPINE_MAX_PLAYERS")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(20),
        }
    }
}

fn live_admission() -> &'static Mutex<Option<LiveAdmission>> {
    static LIVE: OnceLock<Mutex<Option<LiveAdmission>>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(None))
}

/// `login_disconnect` for computed reasons: a ban carries an operator-supplied
/// `reason` string, so the component is built through `json_text` rather than
/// raw interpolation (a `"` or newline in it must not corrupt the JSON).
fn login_disconnect_text(s: &mut TcpStream, reason: &str) {
    let mut pl = Vec::new();
    write_str(&mut pl, &format!("{{\"text\":{}}}", json_text(reason)));
    let _ = send_packet(s, 0, pl);
}

// ---------- W4 live protocol wiring ----------

/// The env-flag spelling the rest of the config surface already uses.
fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| matches!(v.as_str(), "1" | "true" | "on"))
        .unwrap_or(false)
}

/// Opt-in minimap policy (`MC_SPINE_FAIR_PLAY` and `/megu policy`
/// `minimap_mode`, both default **off**). The Xaero fair-play control code
/// `§f§a§i§r§x§a§e§r§o` is only ever emitted when the operator armed the flag
/// or the live policy restricts the map — a client that did not ask for
/// enforcement must never see it. The two `allow` switches record what a
/// fair-play server still permits: they default off under fair-play, on
/// otherwise.
#[derive(Clone, Debug, PartialEq)]
struct MapFairPlayPolicy {
    fair_play: bool,
    allow_cave_map: bool,
    allow_entity_radar: bool,
    /// The `§`-coded Xaero control signal the join sends once, verbatim; an
    /// empty signal means the client stays on its own defaults.
    control: String,
}

fn map_fair_play_policy() -> MapFairPlayPolicy {
    let env_fair_play = env_flag("MC_SPINE_FAIR_PLAY");
    let allow = |name: &str| {
        std::env::var_os(name)
            .map(|_| env_flag(name))
            .unwrap_or(!env_fair_play)
    };
    // POLICY-CMD joins the env knobs: `minimap_mode` `unrestricted` adds
    // nothing, `fair_play` gates each feature on the granular `allow_*`
    // switches and `disabled` is the full lock — the same resolution
    // `server.properties` and the integrated client spell.
    let live = live_server_policy()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let allow_cave_map = allow("MC_SPINE_ALLOW_CAVE_MAP") && live.allow_cave_map;
    let allow_entity_radar =
        allow("MC_SPINE_ALLOW_ENTITY_RADAR") && live.allow_mob_radar;
    let live_restricted = live.minimap_mode != "unrestricted";
    let control = if env_fair_play
        || live.minimap_mode == "disabled"
        || (live_restricted && !allow_cave_map && !allow_entity_radar)
    {
        // The env flag keeps its historical meaning — the full lock — and
        // `disabled` takes the same code: Xaero's protocol has no dedicated
        // disable signal, so the fair-play lock is the strongest there is.
        String::from("§f§a§i§r§x§a§e§r§o")
    } else if !live_restricted {
        String::new()
    } else {
        // `fair_play` with something still allowed: reset, then the
        // selective `§`-code for each feature the policy denies.
        let mut signal = String::from("§r§e§s§e§t§x§a§e§r§o");
        if !allow_cave_map {
            signal.push_str("§n§o§c§a§v§e§m§a§p§x§a§e§r§o");
        }
        if !allow_entity_radar {
            signal.push_str("§n§o§e§n§t§i§t§y§r§a§d§a§r§x§a§e§r§o");
        }
        signal
    };
    MapFairPlayPolicy {
        fair_play: env_fair_play || live_restricted,
        allow_cave_map,
        allow_entity_radar,
        control,
    }
}

/// The system-chat control codes a joining client needs, as encoded `0x6c`
/// payloads. Empty while no restriction applies: default (`unset`/`0` env and
/// `minimap_mode` `unrestricted`) emits zero codes, while an armed env flag,
/// `/megu policy set minimap_mode fair_play|disabled` or a denied `allow_*`
/// switch emits the resolved `§`-coded signal exactly once.
fn map_fair_play_join_chats(policy: &MapFairPlayPolicy) -> Vec<Vec<u8>> {
    if policy.control.is_empty() {
        return Vec::new();
    }
    vec![system_chat(&policy.control)]
}

/// Velocity "modern forwarding" gate (`MC_SPINE_VELOCITY_FORWARDING=1`). When
/// on, the connecting peer must answer `velocity:player_info` with an
/// HMAC-SHA256-signed identity — a direct client, a dropped response or one
/// flipped byte all fail closed, because the forwarded name/UUID/IP is the
/// only trustworthy identity from that point on.
fn velocity_forwarding_enabled() -> bool {
    env_flag("MC_SPINE_VELOCITY_FORWARDING")
}

/// `MC_SPINE_ONLINE_MODE=1`: the Encryption Request/Response exchange plus the
/// session check. Kept `else if` behind forwarding — a Velocity proxy already
/// terminated the client's real authentication, and running both on one
/// connection would contradict that.
fn online_mode_enabled() -> bool {
    env_flag("MC_SPINE_ONLINE_MODE")
}

/// S2C Login Plugin Request (`0x04`) on `velocity:player_info`, then the C2S
/// Plugin Response (`0x02`) verified through `verify_velocity_forwarding`.
/// The `data` of both plugin messages is the remaining frame bytes, not a
/// length-prefixed array.
fn velocity_login(s: &mut TcpStream, deadline: Instant) -> Result<VelocityForwardedPlayer, String> {
    const FORWARDING_MESSAGE_ID: i64 = 0;
    let mut body = Vec::new();
    write_varint(&mut body, FORWARDING_MESSAGE_ID);
    write_str(&mut body, VELOCITY_FORWARDING_CHANNEL);
    // Velocity reads the max modern-forwarding version it may answer with.
    body.push(VELOCITY_FORWARDING_VERSION_MAX as u8);
    send_packet(s, 0x04, body).map_err(|e| format!("plugin request write failed: {e}"))?;
    let (id, resp) = read_login_frame(s, deadline, None)
        .ok_or_else(|| "client left before the plugin response".to_string())?;
    if id != 0x02 {
        return Err(format!("expected plugin response 0x02, got packet {id:#04x}"));
    }
    let mut b = resp.as_slice();
    let mid = read_varint(&mut b).ok_or_else(|| "truncated plugin response".to_string())?;
    if mid != FORWARDING_MESSAGE_ID {
        return Err(format!("plugin response id {mid} does not match the request"));
    }
    let (ok, rest) = b
        .split_first()
        .ok_or_else(|| "truncated plugin response".to_string())?;
    if *ok != 1 {
        return Err("client refused the forwarding request".into());
    }
    if rest.is_empty() {
        return Err("plugin response carried no forwarding data".into());
    }
    let secret = std::env::var("MC_SPINE_VELOCITY_SECRET")
        .map_err(|_| "MC_SPINE_VELOCITY_SECRET is not configured".to_string())?;
    verify_velocity_forwarding(secret.as_bytes(), rest)
}

/// Vanilla online-mode login: Encryption Request (`0x01`), the client's
/// wrapped shared-secret/verify-token answer (`0x01`), the `hasJoined` session
/// check, then the transport switches to AES-128-CFB8 for every later byte.
/// With no installed provider this fails closed — the env flag alone is not a
/// keypair.
fn online_login(
    s: &mut TcpStream,
    name: &str,
    peer: &str,
    deadline: Instant,
) -> Result<MojangJoinedProfile, String> {
    let auth = online_auth_provider()
        .ok_or_else(|| "online mode needs a keypair/session provider".to_string())?;
    let key_der = auth.public_key_der();
    let token = auth.verify_token();
    send_packet(
        s,
        LOGIN_ENCRYPTION_REQUEST_ID,
        encode_encryption_request("", &key_der, &token, true),
    )
    .map_err(|e| format!("encryption request write failed: {e}"))?;
    let (id, body) = read_login_frame(s, deadline, None)
        .ok_or_else(|| "client left before the encryption response".to_string())?;
    if id != LOGIN_ENCRYPTION_RESPONSE_ID {
        return Err(format!("expected encryption response 0x01, got packet {id:#04x}"));
    }
    let (enc_secret, enc_token) = decode_encryption_response(&body)
        .ok_or_else(|| "malformed encryption response".to_string())?;
    let returned_token = auth
        .decrypt_login_field(&enc_token)
        .ok_or_else(|| "verify token did not decrypt".to_string())?;
    if returned_token != token {
        return Err("verify token mismatch".into());
    }
    let secret_vec = auth
        .decrypt_login_field(&enc_secret)
        .ok_or_else(|| "shared secret did not decrypt".to_string())?;
    let secret: [u8; 16] = secret_vec
        .try_into()
        .map_err(|_| "shared secret is not 16 bytes".to_string())?;
    let hash = mc_server_hash("", &secret, &key_der);
    let profile = auth.has_joined(name, &hash, Some(peer))?;
    // Every byte from here on travels under AES-128-CFB8; the play writer
    // (a `try_clone` further down) picks the same cipher up on its own fd.
    enable_conn_encryption(s, &secret);
    Ok(profile)
}

/// The `textures` property a Velocity forward or a Mojang session profile
/// carries, in `Client.skin`'s `(value, signature)` shape.
fn forwarded_skin(properties: &[ForwardedProperty]) -> Option<(String, String)> {
    properties
        .iter()
        .find(|p| p.name == "textures")
        .map(|p| (p.value.clone(), p.signature.clone().unwrap_or_default()))
}

// ---------- mob equipment (S2C 0x5b SetEquipment) ----------

/// Live mob equipment keyed by entity id, so `Mob` literals stay untouched:
/// `default_mob_equipment` decides the loadout the first time an entity's
/// spawn packets are built, and the entry dies with the mob.
fn mob_equipment() -> &'static Mutex<HashMap<i32, EntityEquipment>> {
    static EQUIPMENT: OnceLock<Mutex<HashMap<i32, EntityEquipment>>> = OnceLock::new();
    EQUIPMENT.get_or_init(Mutex::default)
}

/// The `ClientboundSetEquipmentPacket` body for an entity, or `None` when it
/// has nothing worn at all — vanilla requires at least one slot entry on the
/// wire, so a fully empty `EntityEquipment` is simply not announced.
fn entity_equipment_packet(eid: i32, eq: &EntityEquipment) -> Option<Vec<u8>> {
    eq.slots
        .iter()
        .any(|st| st.1 > 0)
        .then(|| encode_set_equipment(eid, eq))
}

/// The 0x5b a fresh observer needs after a mob's spawn packets, or `None` when
/// `default_mob_equipment` leaves the mob bare-handed.
fn mob_equipment_packet(mob: &Mob) -> Option<Vec<u8>> {
    let spec = entity_spec_by_id(mob.etype)?;
    let mut equipment = mob_equipment().lock().unwrap();
    let eq = equipment.entry(mob.eid).or_insert_with(|| {
        default_mob_equipment(
            spec.name,
            DIFFICULTY.load(Ordering::Relaxed),
            world_rng::next_u64(),
        )
    });
    entity_equipment_packet(mob.eid, eq)
}

/// The player's live `EntityEquipment` view of the menu inventory: the main
/// hand is the selected hotbar slot (36..=44), the off hand is menu slot 45,
/// and armor is menu slots 5..=8 in vanilla order head→feet.
fn player_equipment(me: &Client) -> EntityEquipment {
    let inv = me.inv.lock().unwrap();
    let held = *me.held.lock().unwrap();
    let stack = |slot: i16| inv.get(&slot).cloned().unwrap_or((0, 0, Vec::new()));
    let mut eq = EntityEquipment::default();
    eq.set(EquipmentSlot::MainHand, stack(36 + held));
    eq.set(EquipmentSlot::OffHand, stack(45));
    eq.set(EquipmentSlot::Feet, stack(8));
    eq.set(EquipmentSlot::Legs, stack(7));
    eq.set(EquipmentSlot::Chest, stack(6));
    eq.set(EquipmentSlot::Head, stack(5));
    eq
}

/// The 0x5b a fresh observer needs after a player's spawn packet, or `None`
/// when the player holds and wears nothing.
fn player_equipment_packet(me: &Client) -> Option<Vec<u8>> {
    entity_equipment_packet(me.eid, &player_equipment(me))
}

/// A `ClientboundSetEquipmentPacket` body carrying exactly the slots that
/// changed between `before` and `after` — including now-empty stacks, which
/// is how trackers learn a piece came off. `None` when nothing changed, so a
/// fully cleared loadout still announces itself.
fn encode_set_equipment_changes(
    eid: i32,
    before: &EntityEquipment,
    after: &EntityEquipment,
) -> Option<Vec<u8>> {
    let changed: Vec<(u8, &Stack)> = after
        .slots
        .iter()
        .enumerate()
        .filter(|(i, st)| **st != before.slots[*i])
        .map(|(i, st)| (i as u8, st))
        .collect();
    if changed.is_empty() {
        return None;
    }
    let mut p = Vec::new();
    write_varint(&mut p, i64::from(eid));
    for (i, (slot, st)) in changed.iter().enumerate() {
        p.push(slot | if i + 1 < changed.len() { 0x80 } else { 0 });
        p.extend_from_slice(&slot_wire(st));
    }
    Some(p)
}

// ---------- mounts / vehicles (S2C 0x5f, C2S 0x1e/0x1f/0x26) ----------

/// The live `MountRegistry` plus the last paddle flags each boat's driver sent
/// and the last `PlayerInput` each vehicle's driver sent. Keyed globally so
/// `Client`/`Mob` literals in other lanes stay untouched.
#[derive(Default)]
struct LiveMounts {
    registry: MountRegistry,
    paddling: HashMap<i32, PaddleBoatPacket>,
    steering: HashMap<i32, PlayerInputPacket>,
}

fn mounts() -> &'static Mutex<LiveMounts> {
    static MOUNTS: OnceLock<Mutex<LiveMounts>> = OnceLock::new();
    MOUNTS.get_or_init(Mutex::default)
}

/// `entity_types` that can carry a player (`AbstractBoat`, `AbstractMinecart`
/// and the vanilla rideable mobs).
fn vehicle_kind_of(etype: i64) -> Option<VehicleKind> {
    let name = entity_spec_by_id(etype)?.name;
    Some(match name.strip_prefix("minecraft:").unwrap_or(name) {
        "boat" => VehicleKind::Boat,
        "chest_boat" => VehicleKind::ChestBoat,
        "minecart" => VehicleKind::Minecart,
        "horse" => VehicleKind::Horse,
        "donkey" => VehicleKind::Donkey,
        "mule" => VehicleKind::Mule,
        "pig" => VehicleKind::Pig,
        "strider" => VehicleKind::Strider,
        "camel" => VehicleKind::Camel,
        "llama" => VehicleKind::Llama,
        _ => return None,
    })
}

/// A right-click (`interact`/`interact_at`) on a free-seated vehicle starts the
/// ride: vanilla `Entity.startRiding` plus a SetPassengers to every tracker.
fn try_mount_vehicle(me: &Arc<Client>, target: i32) -> bool {
    if me.is_spectator() {
        return false;
    }
    let Some(mob) = ({
        let ms = mobs().lock().unwrap();
        ms.iter().find(|m| m.eid == target).cloned()
    }) else {
        return false;
    };
    let Some(kind) = vehicle_kind_of(mob.etype) else {
        return false;
    };
    let at = {
        let p = *mob.pos.lock().unwrap();
        [p.0, p.1, p.2]
    };
    let seat = {
        let mut live = mounts().lock().unwrap();
        live.registry.register_vehicle(target, kind, at);
        let rider = {
            let p = *me.pos.lock().unwrap();
            [p.0, p.1, p.2]
        };
        live.registry.register_rider(me.eid, rider);
        if live.registry.mount(target, me.eid).is_err() {
            return false;
        }
        live.registry.passengers_of(target).len().saturating_sub(1)
    };
    // `Entity.positionRider`: the passenger snaps to its attachment offset and
    // the carry is credited so anti-cheat does not flag the seat move.
    let off = kind.seat_offset(seat);
    {
        let mut p = me.pos.lock().unwrap();
        *p = (at[0] + off[0], at[1] + off[1], at[2] + off[2], p.3, p.4, p.5);
    }
    ac_note_carried_motion(me.eid, [off[0], off[1], off[2]]);
    let packet = mounts().lock().unwrap().registry.encode_set_passengers(target);
    broadcast_seen(target, |_c| (0x5f, packet.clone()));
    true
}

/// The environment facts `step_vehicle_move` prices a tick at, read where the
/// vehicle actually stands (fluid probes take the entity's own hitbox).
fn live_vehicle_env(dim: DimId, etype: i64, pos: [f64; 3], kind: VehicleKind) -> VehicleEnv {
    let (x, y, z) = (pos[0], pos[1], pos[2]);
    let on_ice = crate::block_registry::breg().is_some_and(|reg| {
        let below = block_at_dim(dim, x.floor() as i32, (y - 0.5).floor() as i32, z.floor() as i32);
        matches!(
            reg.name_of(below),
            "minecraft:ice"
                | "minecraft:packed_ice"
                | "minecraft:blue_ice"
                | "minecraft:frosted_ice"
        )
    });
    let on_rail = crate::block_registry::breg().is_some_and(|reg| {
        [y, y - 1.0].iter().any(|&level| {
            let state = block_at_dim(
                dim,
                x.floor() as i32,
                level.floor() as i32,
                z.floor() as i32,
            );
            matches!(
                reg.name_of(state),
                "minecraft:rail"
                    | "minecraft:powered_rail"
                    | "minecraft:detector_rail"
                    | "minecraft:activator_rail"
            )
        })
    });
    VehicleEnv {
        on_water: mob_in_water(etype, x, y, z),
        on_ice,
        on_lava: mob_in_lava(etype, x, y, z),
        on_rail,
        // Vanilla DefaultAttributes rideable `generic.movement_speed`; per-kind
        // constants here since mob attributes are not instanced per entity yet.
        move_speed: match kind {
            VehicleKind::Horse => 0.225,
            VehicleKind::Pig => 0.25,
            VehicleKind::Camel => 0.09,
            VehicleKind::Donkey | VehicleKind::Mule | VehicleKind::Llama
            | VehicleKind::Strider => 0.175,
            _ => 0.0,
        },
    }
}

/// The controlling passenger's `MoveVehicle` applied to the world: vehicle
/// position plus yaw, every seated rider re-seated, the driver credited for
/// the carry and a `0x70` teleport to the vehicle's trackers.
fn step_live_vehicle(me: &Arc<Client>, pkt: &MoveVehiclePacket) {
    let Some(vehicle) = mounts().lock().unwrap().registry.vehicle_of.get(&me.eid).copied()
    else {
        return;
    };
    let Some(mob) = ({
        let ms = mobs().lock().unwrap();
        ms.iter().find(|m| m.eid == vehicle).cloned()
    }) else {
        return;
    };
    let Some(kind) = vehicle_kind_of(mob.etype) else {
        return;
    };
    let dim = *me.dim.lock().unwrap();
    let from = {
        let p = *mob.pos.lock().unwrap();
        [p.0, p.1, p.2]
    };
    let env = live_vehicle_env(dim, mob.etype, from, kind);
    let step = {
        let mut live = mounts().lock().unwrap();
        // The registry's last word on the vehicle position is what the vanilla
        // anti-cheat distance check prices the step against.
        live.registry.register_rider(vehicle, from);
        live.registry.step_vehicle_move(vehicle, pkt, &env)
    };
    let Ok(step) = step else { return };
    {
        let mut p = mob.pos.lock().unwrap();
        p.0 = step.pos[0];
        p.1 = step.pos[1];
        p.2 = step.pos[2];
        p.3 = pkt.yaw;
        p.4 = pkt.pitch;
    }
    ac_note_carried_motion(
        me.eid,
        [step.carry_delta.0, step.carry_delta.1, step.carry_delta.2],
    );
    let seat = mounts()
        .lock()
        .unwrap()
        .registry
        .passengers_of(vehicle)
        .iter()
        .position(|&e| e == me.eid);
    if let Some(seat) = seat {
        let off = kind.seat_offset(seat);
        let mut p = me.pos.lock().unwrap();
        let (yaw, pitch, ground) = (p.3, p.4, p.5);
        *p = (
            step.pos[0] + off[0],
            step.pos[1] + off[1],
            step.pos[2] + off[2],
            yaw,
            pitch,
            ground,
        );
    }
    let packet = entity_teleport(vehicle, step.pos[0], step.pos[1], step.pos[2], pkt.yaw, pkt.pitch, true);
    broadcast_seen(vehicle, |_c| (0x70, packet.clone()));
}

/// The vehicle a passenger eid is riding, per the live mount graph.
fn vehicle_of(eid: i32) -> Option<i32> {
    mounts().lock().unwrap().registry.vehicle_of.get(&eid).copied()
}

/// Dismount a riding player, re-seat them at the offset vanilla
/// `Entity.stopRiding` computes, and tell the vehicle's trackers.
fn player_dismount(me: &Arc<Client>, reposition: bool) {
    let Some(vehicle) = vehicle_of(me.eid) else {
        return;
    };
    let at = mounts().lock().unwrap().registry.dismount(me.eid);
    let Some(at) = at else { return };
    if reposition {
        let (yaw, pitch) = {
            let p = me.pos.lock().unwrap();
            (p.3, p.4)
        };
        let mut sock = me.sock.lock().unwrap();
        teleport_in_dimension(me, &mut sock, (at[0], at[1], at[2], yaw, pitch));
    } else {
        let mut p = me.pos.lock().unwrap();
        let (yaw, pitch, ground) = (p.3, p.4, p.5);
        *p = (at[0], at[1], at[2], yaw, pitch, ground);
    }
    let packet = mounts().lock().unwrap().registry.encode_set_passengers(vehicle);
    broadcast_seen(vehicle, |_c| (0x5f, packet.clone()));
}

/// Entity teardown for the mount graph (`Entity.remove`/`die` plus player
/// disconnect): a dead vehicle frees its riders and a dead rider leaves its
/// seat; every vehicle whose seats changed sends a fresh SetPassengers.
fn live_mount_forget(eid: i32) {
    let mut freed: Vec<(i32, [f64; 3])> = Vec::new();
    let mut updated: Vec<i32> = Vec::new();
    {
        let mut live = mounts().lock().unwrap();
        let seats = live.registry.passengers_of(eid).to_vec();
        if !seats.is_empty() {
            for passenger in seats {
                if let Some(at) = live.registry.dismount(passenger) {
                    freed.push((passenger, at));
                }
            }
            updated.push(eid);
        }
        if let Some(vehicle) = live.registry.vehicle_of.get(&eid).copied() {
            if live.registry.dismount(eid).is_some() {
                updated.push(vehicle);
            }
        }
        live.paddling.remove(&eid);
        live.steering.remove(&eid);
    }
    for (passenger, at) in freed {
        if let Some(client) = clients()
            .lock()
            .unwrap()
            .iter()
            .find(|c| c.eid == passenger)
            .cloned()
        {
            let mut p = client.pos.lock().unwrap();
            let (yaw, pitch, ground) = (p.3, p.4, p.5);
            *p = (at[0], at[1], at[2], yaw, pitch, ground);
        }
    }
    updated.sort_unstable();
    updated.dedup();
    for vehicle in updated {
        let packet = mounts().lock().unwrap().registry.encode_set_passengers(vehicle);
        broadcast_seen(vehicle, |_c| (0x5f, packet.clone()));
    }
}

// ---------- P7a / #132: live signed chat ----------

/// The `minecraft:chat` entry of the `chat_type` registry — index 0 in the
/// vanilla ordering the config-phase registry sync replays.
fn live_chat_type_index() -> i32 {
    0
}

/// Vanilla's signed-chat chain index: a server-global counter stamped on
/// every `0x39` Player Chat the relay forwards.
fn next_chat_message_index() -> i32 {
    static INDEX: AtomicI32 = AtomicI32::new(0);
    INDEX.fetch_add(1, Ordering::Relaxed)
}

/// The enforce-secure-profile admission check for one decoded envelope.
fn live_chat_validate(
    me: &Arc<Client>,
    env: &ChatMessageEnvelope,
) -> Result<ChatVerdict, ChatRejectReason> {
    let policy = chat_signature_policy();
    let is_op = live_admission()
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|admission| admission.ops.is_op(&me.name));
    validate_chat_envelope_as(env, &policy, online_mode_enabled(), is_op)
}

/// Relay one admitted envelope: `0x39` Player Chat forwards the signature
/// end-to-end; an unsigned line goes out as `0x1e` Disguised Chat — the
/// vanilla relay path — with the sender-decorated component as its content.
fn live_chat_relay(me: &Arc<Client>, env: &ChatMessageEnvelope, m: &str, verdict: ChatVerdict) {
    match verdict {
        ChatVerdict::Signed => {
            let frame = encode_player_chat_0x39(
                env,
                next_chat_message_index(),
                None,
                live_chat_type_index(),
                &me.name,
            );
            // The encoder emits `id + body`; `send_packet` wants them split.
            let mut body = frame.as_slice();
            let id = read_varint(&mut body).unwrap_or(S2C_PLAYER_CHAT_767);
            let payload = body.to_vec();
            let _ = send_packet(&mut me.sock.lock().unwrap_or_else(|e| e.into_inner()), id, payload.clone());
            broadcast(me.eid, |_c| (id, payload.clone()));
        }
        ChatVerdict::Disguised => {
            let frame = {
                // Deep indent keeps the line decoration byte-identical for
                // the mutation anchor that pins this exact statement.
                    let line = format!("<{}> {}", me.name, m);
                encode_disguised_chat_0x1e(&chat_nbt(&line), live_chat_type_index(), &me.name)
            };
            let mut body = frame.as_slice();
            let id = read_varint(&mut body).unwrap_or(S2C_DISGUISED_CHAT_767);
            let payload = body.to_vec();
            let _ = send_packet(&mut me.sock.lock().unwrap_or_else(|e| e.into_inner()), id, payload.clone());
            broadcast(me.eid, |_c| (id, payload.clone()));
        }
    }
}

/// The live `0x06` chat pipeline: parse the protocol-767 signed envelope,
/// run the admission gate (`validate_chat_envelope_as` against the
/// enforce-secure-profile policy), and relay per the verdict. `Err(text)` is
/// the exact line the connection handler disconnects the sender with.
fn live_chat_message(
    me: &Arc<Client>,
    body: &[u8],
    chat_throttle: &mut connection_limits::ChatThrottle,
) -> Result<(), &'static str> {
    let env = decode_serverbound_chat_0x06(me.uuid, body)
        .map_err(|reason| reason.disconnect_text())?;
    let m = env.message.clone();
    // vanilla: over-long or illegal text disconnects, and so does spam
    if !connection_limits::chat_text_valid(&m) {
        return Err("Illegal characters in chat");
    }
    if !chat_throttle.allow(Instant::now()) {
        return Err("Kicked for spamming!");
    }
    if m.starts_with('/') {
        run_command(Some(me), &m, &mut Some(me));
        return Ok(());
    }
    #[cfg(feature = "neoforge-events")]
    let m = if java_compat::NEOFORGE.get().is_some() {
        match neoforge_event(me, java_compat::S1Event::ServerChat {
            player: (me.eid, me.name.clone()),
            raw_text: m.clone(),
        }) {
            Ok(rewritten) => rewritten.unwrap_or(m),
            Err(_) => return Ok(()),
        }
    } else {
        m
    };
    let verdict = live_chat_validate(me, &env).map_err(|reason| reason.disconnect_text())?;
    live_chat_relay(me, &env, &m, verdict);
    Ok(())
}

// ---------- recipe book / statistics (0x77/0x41/0x28/0x29/0x22/0x04/0x6d) ----------

/// Per-player recipe books keyed by player uuid — `Client` literals stay
/// untouched across lanes.
fn recipe_books() -> &'static Mutex<HashMap<[u8; 16], PlayerRecipeBook>> {
    static BOOKS: OnceLock<Mutex<HashMap<[u8; 16], PlayerRecipeBook>>> = OnceLock::new();
    BOOKS.get_or_init(Mutex::default)
}

/// Per-player `StatsCounter` (S2C 0x04 AwardStats source).
fn player_stats() -> &'static Mutex<HashMap<[u8; 16], PlayerStats>> {
    static STATS: OnceLock<Mutex<HashMap<[u8; 16], PlayerStats>>> = OnceLock::new();
    STATS.get_or_init(Mutex::default)
}

/// `StatsCounter.increment` for a live client.
fn stat_increment(me: &Client, category: StatCategory, stat_id: i32, delta: i32) {
    player_stats()
        .lock()
        .unwrap()
        .entry(me.uuid)
        .or_insert_with(PlayerStats::new)
        .increment(category, stat_id, delta);
}

/// The recipe ids the book reports as unlocked at join plus the unlocks the
/// runtime grants — the 0x41 INIT `recipes` list source.
fn player_recipe_book(uuid: [u8; 16]) -> PlayerRecipeBook {
    recipe_books()
        .lock()
        .unwrap()
        .entry(uuid)
        .or_insert_with(PlayerRecipeBook::new)
        .clone()
}

/// The server's stonecutting universe as 0x77 `RecipeHolder` entries —
/// `wm_stonecut_recipes` is also the table the live stonecutter menu cuts
/// against, so the recipe list and the menu agree. Datapack crafting/smelting
/// recipes do not carry their ResourceLocation ids in memory yet, so only the
/// built-in stonecutting set is listed here.
fn update_recipes_entries() -> Vec<RbsRecipeEntry> {
    let mut entries = Vec::new();
    let mut inputs: Vec<String> = WM_STONECUT.iter().map(|(n, _)| n.to_string()).collect();
    for prefix in WM_COPPER_PREFIXES {
        inputs.push(format!("{prefix}copper_block"));
        inputs.push(format!("{prefix}cut_copper"));
    }
    for input in inputs {
        let Some(input_item) = name_to_item(&input) else {
            continue;
        };
        for (item, count) in wm_stonecut_recipes(&input) {
            let Some(out_name) = item_to_name(item) else {
                continue;
            };
            entries.push(RbsRecipeEntry {
                id: format!("minecraft:{out_name}_from_{input}_stonecutting"),
                recipe: RbsRecipe::Stonecutting {
                    group: String::new(),
                    ingredient: vec![RbsSlot::simple(input_item as i32, 1)],
                    result: RbsSlot::simple(item as i32, i32::from(count)),
                },
            });
        }
    }
    entries
}

/// The craft-grid recipes `handlePlaceRecipe` can lay out — a bounded set that
/// matches what `craft_result`/`craft3_result` actually produce, so a placed
/// pattern immediately shows its result slot. Cells are row-major; `""` is
/// air, `"#planks"`/`"#logs"` match the tag, anything else is an item name.
struct W3PlaceRecipe {
    id: &'static str,
    pattern: &'static [&'static str],
    width: usize,
    needs_3x3: bool,
}

const W3_PLACE_RECIPES: &[W3PlaceRecipe] = &[
    W3PlaceRecipe {
        id: "minecraft:crafting_table",
        pattern: &["#planks", "#planks", "#planks", "#planks"],
        width: 2,
        needs_3x3: false,
    },
    W3PlaceRecipe {
        id: "minecraft:stick",
        pattern: &["#planks", "", "#planks", ""],
        width: 2,
        needs_3x3: false,
    },
    W3PlaceRecipe {
        id: "minecraft:torch",
        pattern: &["coal", "", "stick", ""],
        width: 2,
        needs_3x3: false,
    },
    W3PlaceRecipe {
        id: "minecraft:oak_planks",
        pattern: &["#logs"],
        width: 1,
        needs_3x3: false,
    },
    W3PlaceRecipe {
        id: "minecraft:chest",
        pattern: &[
            "#planks", "#planks", "#planks", "#planks", "", "#planks", "#planks", "#planks",
            "#planks",
        ],
        width: 3,
        needs_3x3: true,
    },
    W3PlaceRecipe {
        id: "minecraft:furnace",
        pattern: &[
            "cobblestone", "cobblestone", "cobblestone", "cobblestone", "", "cobblestone",
            "cobblestone", "cobblestone", "cobblestone",
        ],
        width: 3,
        needs_3x3: true,
    },
    W3PlaceRecipe {
        id: "minecraft:bread",
        pattern: &["wheat", "wheat", "wheat"],
        width: 3,
        needs_3x3: true,
    },
];

fn w3_ingredient_matches(matcher: &str, item: i64) -> bool {
    match matcher {
        "#planks" => tag_planks(item),
        "#logs" => planks_for_log(item).is_some(),
        name => name_to_item(name) == Some(item),
    }
}

/// `ServerGamePacketListenerImpl.handlePlaceRecipe`: the client asks the server
/// to lay an unlocked recipe out over its craft grid, one or (`makeAll`) as
/// many sets as the inventory feeds. Unknown or locked recipes are ignored the
/// same way vanilla's `ServerRecipeBook.contains` check drops them.
fn place_live_recipe(me: &Arc<Client>, place: &PlaceRecipe) {
    let grid_is_3x3 = if place.window_id == 0 {
        false
    } else if place.window_id == 3 && *me.open_table.lock().unwrap() {
        true
    } else {
        return;
    };
    let Some(recipe) = W3_PLACE_RECIPES
        .iter()
        .find(|r| r.id == place.recipe_id)
    else {
        return;
    };
    if recipe.needs_3x3 && !grid_is_3x3 {
        return;
    }
    if !recipe_books()
        .lock()
        .unwrap()
        .get(&me.uuid)
        .is_some_and(|book| book.is_unlocked(&place.recipe_id))
    {
        return;
    }
    let dim = if grid_is_3x3 { 3usize } else { 2 };
    let wants: Vec<(i16, &str)> = recipe
        .pattern
        .iter()
        .enumerate()
        .filter(|(_, cell)| !cell.is_empty())
        .map(|(i, cell)| (((i / recipe.width) * dim + i % recipe.width + 1) as i16, *cell))
        .collect();
    let repeats = if place.make_all { 64 } else { 1 };
    let mut placed = false;
    {
        let mut inv = me.inv.lock().unwrap();
        let mut grid = if grid_is_3x3 {
            me.craft9.lock().unwrap()
        } else {
            me.craft.lock().unwrap()
        };
        for _ in 0..repeats {
            // Commit-or-nothing one repeat: try the whole pattern on copies so
            // a half-empty inventory never leaves a partial row in the grid.
            let mut try_inv = inv.clone();
            let mut try_grid = grid.clone();
            let mut fits = true;
            for &(slot, matcher) in &wants {
                let takes = try_grid.get(&slot).is_none_or(|st| st.1 == 0)
                    || try_grid.get(&slot).is_some_and(|st| {
                        w3_ingredient_matches(matcher, st.0)
                            && i64::from(st.1) < item_stack_max(st.0, &st.2)
                    });
                let pick = if takes {
                    try_inv
                        .iter()
                        .find(|(_, st)| st.1 > 0 && w3_ingredient_matches(matcher, st.0))
                        .map(|(&islot, st)| (islot, st.0, st.2.clone()))
                } else {
                    None
                };
                let Some((islot, item, comps)) = pick else {
                    fits = false;
                    break;
                };
                let st = try_inv.get_mut(&islot).unwrap();
                st.1 -= 1;
                if st.1 == 0 {
                    try_inv.remove(&islot);
                }
                let cur = try_grid.entry(slot).or_insert((item, 0, comps));
                cur.1 += 1;
            }
            if !fits {
                break;
            }
            *inv = try_inv;
            *grid = try_grid;
            placed = true;
        }
        if !placed {
            return;
        }
    }
    // Mirror the click handler's post-change echoes: the recipe result, the
    // grid slots and the inventory slots the ingredients came from.
    let (grid, inv) = (
        if grid_is_3x3 {
            me.craft9.lock().unwrap().clone()
        } else {
            me.craft.lock().unwrap().clone()
        },
        me.inv.lock().unwrap().clone(),
    );
    let result = if grid_is_3x3 {
        craft3_result(&grid)
    } else {
        craft_result(&grid)
    }
    .unwrap_or((0, 0, Vec::new()));
    let wid = place.window_id as u8;
    let mut sock = me.sock.lock().unwrap();
    let mut echo = |slot: i16, st: &Stack| {
        let mut p = vec![wid];
        write_varint(&mut p, 0);
        p.extend_from_slice(&slot.to_be_bytes());
        p.extend_from_slice(&slot_wire(st));
        let _ = send_packet(&mut sock, 0x15, p);
    };
    echo(0, &result);
    let grid_len = if grid_is_3x3 { 9 } else { 4 };
    for slot in 1..=grid_len {
        echo(
            slot,
            &grid.get(&slot).cloned().unwrap_or((0, 0, Vec::new())),
        );
    }
    // window slot -> inventory key: window 0's inventory cells share the map's
    // indices; the 3x3 table offsets them by one column of craft slots.
    let offset = if grid_is_3x3 { 1 } else { 0 };
    for slot in 9..=44i16 {
        let st = inv.get(&slot).cloned().unwrap_or((0, 0, Vec::new()));
        echo(slot + offset, &st);
    }
}

/// Entry point for one accepted socket (`main` spawns one thread per call).
/// The whole connection body runs inside `run_isolated_connection`: a panic in
/// any packet handler becomes one logged, counted disconnect — the listener
/// and every other client keep running — instead of taking the process down.
fn handle(s: TcpStream) {
    let label = s
        .peer_addr()
        .map_or_else(|_| "unknown".to_string(), |a| a.to_string());
    // The play eid this connection registers, shared with the panic cleanup so
    // a mid-session panic still unregisters the client and its mount state.
    let live_eid = Arc::new(AtomicI32::new(-1));
    let cleanup_eid = Arc::clone(&live_eid);
    let _ = run_isolated_connection(
        &label,
        move || handle_inner(s, live_eid),
        move || {
            let eid = cleanup_eid.load(Ordering::SeqCst);
            if eid >= 0 {
                forget_live_client(eid);
            }
        },
    );
}

/// The play-state part of a connection's teardown when the handler panics
/// mid-session: unregister the client from the online list and detach it from
/// the mount graph like the normal disconnect path does.
fn forget_live_client(eid: i32) {
    clients().lock().unwrap().retain(|c| c.eid != eid);
    live_mount_forget(eid);
}

fn handle_inner(mut s: TcpStream, live_eid: Arc<AtomicI32>) {
    s.set_nodelay(true).ok();
    let mut peer = s
        .peer_addr()
        .map_or_else(|_| "unknown".to_string(), |a| a.ip().to_string());
    // Applied before handshake/login: idle unauthenticated sockets must expire.
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    s.set_write_timeout(Some(Duration::from_secs(10))).ok();
    // One wall-clock budget for handshake, login and configuration together.
    let login_deadline = Instant::now() + connection_limits::login_timeout();
    let (id, hs) = match read_packet(&mut s, login_deadline) {
        Some(x) => x,
        None => return,
    };
    if id != 0 {
        return;
    }
    let Some((protocol, next)) = handshake_parts(&hs) else {
        return;
    };
    if protocol != 767 {
        let reason = if protocol < 767 { "Outdated client! Please use 1.21.1." } else { "Outdated server! Please use 1.21.1." };
        if next == 1 {
            status_version_mismatch(&mut s, reason, login_deadline);
        } else {
            login_disconnect(&mut s, reason);
        }
        return;
    }
    if next == 1 {
        if let Some((0, _)) = read_packet(&mut s, login_deadline) {
            let online_players: Vec<(String, [u8; 16])> = clients()
                .lock()
                .unwrap()
                .iter()
                .map(|c| (c.name.clone(), c.uuid))
                .collect();
            let max = std::env::var("MC_SPINE_MAX_PLAYERS").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(20);
            let motd = std::env::var("MC_SPINE_MOTD").unwrap_or_else(|_| "mc-spine".to_owned());
            let json = build_status_json_rich(&motd, max, &online_players, status_favicon());
            let mut pl = Vec::new();
            write_str(&mut pl, &json);
            let _ = send_packet(&mut s, 0, pl);
        }
        if let Some((1, ping)) = read_packet(&mut s, login_deadline) {
            let _ = send_packet(&mut s, 1, ping);
        }
        return;
    }
    if next != 2 {
        return;
    }
    // ---- login ----
    let (lid, lbody) = match read_packet(&mut s, login_deadline) {
        Some(x) => x,
        None => return,
    };
    if lid != 0 {
        return;
    }
    let Some((mut name, mut uuid)) = login_identity(&lbody) else {
        return;
    };
    // W4: identity-bearing login exchanges run before admission, so the ban and
    // whitelist checks judge the *verified* identity — the Velocity-forwarded
    // triple or the Mojang session profile — not whatever the socket claims.
    let mut login_skin = None;
    if velocity_forwarding_enabled() {
        match velocity_login(&mut s, login_deadline) {
            Ok(fwd) => {
                eprintln!("[spine] {peer} forwarded {} as {}", fwd.forwarded_ip, fwd.username);
                peer = fwd.forwarded_ip;
                uuid = fwd.uuid;
                name = fwd.username;
                login_skin = forwarded_skin(&fwd.properties);
            }
            Err(why) => {
                eprintln!("[security] velocity forwarding refused for {name} from {peer}: {why}");
                login_disconnect_text(&mut s, "This server requires connections through the proxy.");
                return;
            }
        }
    } else if online_mode_enabled() {
        match online_login(&mut s, &name, &peer, login_deadline) {
            Ok(profile) => {
                uuid = profile.uuid;
                name = profile.name;
                login_skin = forwarded_skin(&profile.properties);
            }
            Err(why) => {
                eprintln!("[security] online-mode login refused for {name} from {peer}: {why}");
                login_disconnect(&mut s, "Failed to verify username!");
                return;
            }
        }
    }
    // `LoginCompression` only guards the wire entry when a threshold was
    // negotiated; a cipher armed above must not outlive this connection, or a
    // later connection reusing the fd would read a dead keystream.
    let _cipher_guard = conn_cipher(&s).map(|_| SocketCipherGuard(socket_key(&s)));
    if let Some(list) = connection_limits::allowlist() {
        if !list.permits(&name, &uuid) {
            eprintln!("[security] login refused for {name} from {peer}: not on the allowlist");
            login_disconnect(&mut s, "You are not white-listed on this server!");
            return;
        }
    }
    // Vanilla `PlayerList.canPlayerLogin`: IP ban, player ban, whitelist, then
    // the player cap — evaluated before Login Success so the refusal arrives
    // as a Login-state Disconnect like Vanilla's.
    {
        let online = clients().lock().unwrap().len();
        let mut live = live_admission().lock().unwrap();
        let live = live.get_or_insert_with(LiveAdmission::load);
        if let Err(reason) = check_login_admission(
            &name,
            &peer,
            online,
            live.max_players,
            &live.ops,
            &live.bans,
            &live.whitelist,
        ) {
            eprintln!("[security] login refused for {name} from {peer}: {reason}");
            login_disconnect_text(&mut s, &reason);
            return;
        }
    }
    // Held until this function returns, i.e. through the player's save on
    // logout: a second session under the same identity cannot start from stale
    // saved state or write over this one.
    let _login = match LOGINS.reserve(&name, uuid) {
        Ok(g) => g,
        Err(why) => {
            eprintln!("[security] duplicate login refused for {name} from {peer}: {why:?}");
            login_disconnect(
                &mut s,
                "A player with that name or UUID is already logged in.",
            );
            return;
        }
    };
    // Vanilla 1.21.1 compresses at 256 bytes by default; an explicit `-1`/`0`
    // in the env keeps framing uncompressed. In-process live-TCP fixtures
    // drive `handle` with a raw uncompressed client, so the test build keeps
    // the old disabled default unless the env names a threshold.
    let threshold =
        connection_limits::compression_threshold_env(|name| std::env::var(name).ok());
    let threshold = if cfg!(test)
        && std::env::var_os("MC_SPINE_COMPRESSION_THRESHOLD").is_none()
    {
        -1
    } else {
        threshold
    };
    let Ok(_compression_registration) = negotiate_compression(&mut s, threshold) else {
        return;
    };
    let compression = usize::try_from(threshold).ok();
    let mut ls = Vec::new();
    ls.extend_from_slice(&uuid);
    write_str(&mut ls, &name);
    write_varint(&mut ls, 0);
    ls.push(0);
    if send_packet(&mut s, 2, ls).is_err() {
        return;
    }
    match read_login_frame(&mut s, login_deadline, compression) {
        Some((3, _)) => {}
        _ => return,
    }
    // ---- config ----
    let mut client_view: i32 = 8;
    let cfg = load_config_packets();
    for (pid, pkt) in &cfg {
        if write_frame(&mut s, pkt, compression).is_err() {
            return;
        }
        if *pid == 0x0e {
            break;
        }
    }
    if !cfg.iter().any(|(id, _)| *id == 0x0e) {
        return;
    }
    s.set_read_timeout(Some(Duration::from_millis(200))).ok();
    let t0 = Instant::now();
    let mut got_selpacks = false;
    while Instant::now() - t0 < Duration::from_secs(15) {
        match read_login_frame(&mut s, login_deadline, compression) {
            Some((0x00, b)) => {
                let Some(v) = client_info_view_distance(&b) else {
                    return;
                };
                client_view = v;
            }
            Some((0x07, _)) => {
                got_selpacks = true;
                break;
            }
            Some(_) => continue,
            // EOF, malformed length or the login deadline: the client is gone.
            // `continue` here spun at 100% CPU, since read_packet fails at once.
            None => return,
        }
    }
    if !got_selpacks {
        return;
    }
    for (pid, pkt) in &cfg {
        if matches!(*pid, 0x01 | 0x0c | 0x0e | 0x03) {
            continue;
        }
        if write_frame(&mut s, pkt, compression).is_err() {
            return;
        }
    }
    if send_packet(&mut s, 0x03, vec![]).is_err() {
        return;
    }
    let t0 = Instant::now();
    loop {
        if Instant::now() - t0 > Duration::from_secs(15) {
            return;
        }
        match read_login_frame(&mut s, login_deadline, compression) {
            Some((0x00, b)) => {
                let Some(v) = client_info_view_distance(&b) else {
                    return;
                };
                client_view = v;
            }
            Some((0x03, _)) => break,
            Some(_) => continue,
            None => return,
        }
    }
    // ---- play ----
    // P2-3-S4: read the saved dimension BEFORE 0x2b so Join Game and the first chunk
    // window both name the world the player actually logged out in. The 0x2b payload and
    // the camera position must agree or the client asks for chunks from the wrong generator.
    let player_uuid: [u8; 16] = uuid.as_slice().try_into().unwrap();
    let saved_player = saved_players().lock().unwrap().get(&player_uuid).cloned();
    let (saved_dim, sx, sy, sz, syaw, spitch) = saved_player
        .as_ref()
        .map(|save| save.position)
        .or_else(|| pos_store().lock().unwrap().get(&name).copied())
        .unwrap_or_else(|| {
            let w = *WORLD_SPAWN.lock().unwrap();
            (DimId::Overworld, w.0, w.1, w.2, w.3, 0.0)
        });
    let player_eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
    let initial_gm = saved_player
        .as_ref()
        .map_or_else(|| DEFAULT_GAME_MODE.load(Ordering::Relaxed), |save| save.gamemode);
    let initial_held = saved_player.as_ref().map_or(0, |save| save.held.clamp(0, 8));
    if send_packet(&mut s, 0x2b, join_game(saved_dim, initial_gm, player_eid)).is_err() {
        return;
    }
    send_view_distance(&mut s, client_view.clamp(2, SERVER_VIEW_DISTANCE));
    let login_admin = player_is_admin(&name);
    if send_commands(&mut s, login_admin).is_err() {
        return;
    }
    let _ = send_packet(&mut s, 0x53, vec![initial_held as u8]);
    let mut vp = Vec::new();
    write_varint(&mut vp, i64::from((sx.floor() as i32).div_euclid(16)));
    write_varint(&mut vp, i64::from((sz.floor() as i32).div_euclid(16)));
    let _ = send_packet(&mut s, 0x54, vp);
    let spawn = *WORLD_SPAWN.lock().unwrap();
    let _ = send_packet(&mut s, 0x56, spawn_position_packet(spawn));
    let mut gs = vec![13u8];
    gs.extend_from_slice(&0f32.to_be_bytes());
    let _ = send_packet(&mut s, 0x22, gs);
    if RAINING.load(Ordering::Relaxed) > 0 {
        let mut rp = vec![1u8];
        rp.extend_from_slice(&0f32.to_be_bytes());
        let _ = send_packet(&mut s, 0x22, rp);
        if THUNDERING.load(Ordering::Relaxed) > 0 {
            let mut tp = vec![8u8];
            tp.extend_from_slice(&1f32.to_be_bytes());
            let _ = send_packet(&mut s, 0x22, tp);
        }
    }
    let mut sent: std::collections::HashSet<(i32, i32)> = std::collections::HashSet::new();
    let mut gen: GenSources = GenSources::new();
    let mut view = client_view.clamp(2, SERVER_VIEW_DISTANCE);
    let mut chunk_ack = connection_limits::chunk_feedback(window_batch(view));
    let mut chunk_delivery = connection_limits::ChunkDelivery::default();
    let initial_window = ChunkWindow::at(sx, sz, view);
    // vanilla: a returning player resumes where they logged out — the chunk window,
    // spawn teleport and authoritative pos all agree. The dimension was read above,
    // before Join Game, and is threaded through the first stream as well. The join
    // lane starts occluded unless the saved position is already underground.
    let initial_occluded = should_occlude_underground(
        saved_dim,
        sy,
        height_at_dim(saved_dim, sx.floor() as i32, sz.floor() as i32),
        initial_gm,
        false,
    );
    stream_chunks(
        &mut s,
        &mut sent,
        &mut gen,
        &mut chunk_ack,
        saved_dim,
        initial_window,
        syaw,
        true,
        Some(&mut chunk_delivery),
        initial_occluded,
    );
    let mut tm = Vec::new();
    tm.extend_from_slice(&0i64.to_be_bytes());
    tm.extend_from_slice(&6000i64.to_be_bytes());
    let _ = send_packet(&mut s, 0x64, tm);
    let mut teleport = connection_limits::TeleportSync::default();
    let initial_id = teleport.begin();
    send_prepared_teleport(&mut s, &mut teleport, (sx, sy, sz, syaw, spitch), initial_id);
    let mut ab = vec![0x0du8];
    ab.extend_from_slice(&0.05f32.to_be_bytes());
    ab.extend_from_slice(&0.1f32.to_be_bytes());
    let _ = send_packet(&mut s, 0x38, ab);
    let _ = send_packet(
        &mut s,
        0x0b,
        vec![DIFFICULTY.load(Ordering::Relaxed), 0],
    );
    let mut xp = Vec::new();
    xp.extend_from_slice(&saved_player.as_ref().map_or(0.0, |save| save.xp_progress).to_be_bytes());
    write_varint(&mut xp, saved_player.as_ref().map_or(0, |save| save.xp_level) as i64);
    write_varint(&mut xp, saved_player.as_ref().map_or(0, |save| save.xp_total));
    let _ = send_packet(&mut s, 0x5c, xp);
    let mut hp = Vec::new();
    hp.extend_from_slice(&saved_player.as_ref().map_or_else(|| max_hp_of(&name), |save| save.health).to_be_bytes());
    write_varint(&mut hp, saved_player.as_ref().map_or(20, |save| save.food) as i64);
    hp.extend_from_slice(&saved_player.as_ref().map_or(5.0, |save| save.saturation).to_be_bytes());
    let _ = send_packet(&mut s, 0x5d, hp);
    // Live mod suite: the armed `mc-spine:mod_sync_v1` (MSY1) manifest rides
    // the play-phase 0x19 custom payload. No-op unless an operator manifest
    // validated at boot (default builds send nothing new).
    live_mod_sync_on_join(&mut s);
    if let Some(rp) = resource_pack_pkt() {
        let _ = send_packet(&mut s, 0x46, rp);
    }
    // `PlayerList.placeNewPlayer` recipe-book order: UpdateRecipes, then the
    // recipe-book INIT carrying the player's settings and highlights; the
    // tab-list header/footer pair is opt-in like Vanilla's dedicated server
    // files rather than the single `header` key some forks expose.
    {
        let book = player_recipe_book(player_uuid);
        let _ = send_packet(&mut s, 0x77, encode_update_recipes(&update_recipes_entries()));
        let init = UpdateRecipesBook {
            action: RecipeBookAction::Init,
            settings: book.settings,
            recipes: book.unlocked.iter().cloned().collect(),
            to_highlight: book.to_highlight(),
        };
        let _ = send_packet(&mut s, 0x41, encode_update_recipes_book(&init));
        if let Ok(header) = std::env::var("MC_SPINE_TABLIST_HEADER") {
            let footer = std::env::var("MC_SPINE_TABLIST_FOOTER").unwrap_or_default();
            let _ = send_packet(&mut s, 0x6d, encode_tab_list(&header, &footer));
        }
    }
    // restore saved inventory for this name
    let saved_inv = saved_player
        .as_ref()
        .map(|save| save.inventory.clone())
        .or_else(|| inv_store().lock().unwrap().get(&name).cloned());
    eprintln!(
        "[spine] {} joined from {} (uuid {})",
        name,
        peer,
        hex(&uuid)
    );
    // ---- multiplayer: register self, tell others, learn others ----
    let Ok(writer) = s.try_clone() else { return };
    // The play writer is a second fd on the same socket; the cipher armed on
    // `s` during the encryption handshake must cover it or broadcasts to this
    // client would leave in the clear.
    if let Some(cipher) = conn_cipher(&s) {
        socket_wire()
            .lock()
            .unwrap()
            .entry(socket_key(&writer))
            .or_default()
            .cipher = Some(cipher);
    }
    let me = Arc::new(Client {
        eid: player_eid,
        uuid: player_uuid,
        name: name.clone(),
        player_attack_damage_id: player_attack_damage_wire_id(&cfg),
        pos: Mutex::new((sx, sy, sz, syaw, spitch, true)),
        teleport: Mutex::new(teleport),
        hp: Mutex::new(saved_player.as_ref().map_or(20.0, |save| save.health)),
        sock: Mutex::new(writer),
        inv: Mutex::new(saved_inv.unwrap_or_default()),
        cursor: Mutex::new((0, 0, Vec::new())),
        drag: Mutex::new((-1, Vec::new())),
        open_chest: Mutex::new(None),
        chest_sync: Mutex::new(None),
        open_furnace: Mutex::new(None),
        craft: Mutex::new(HashMap::new()),
        craft9: Mutex::new(HashMap::new()),
        open_table: Mutex::new(false),
        #[cfg(test)]
        open_merchant: Mutex::new(false),
        #[cfg(test)]
        trade_sel: Mutex::new(-1),
        #[cfg(test)]
        merchant_in: Mutex::new(Default::default()),
        bow_draw: Mutex::new(None),
        dig: Mutex::new(None),
        eat_start: Mutex::new(None),
        blocking: Mutex::new(false),
        sprinting: Mutex::new(false),
        sneaking: Mutex::new(false),
        last_melee: Mutex::new(Instant::now() - Duration::from_secs(10)),
        fall_fly: Mutex::new(false),
        invulnerable: Mutex::new(0),
        last_hurt: Mutex::new(0.0),
        shield_cd: Mutex::new(Instant::now() - Duration::from_secs(10)),
        absorb: Mutex::new(saved_player.as_ref().map_or(0.0, |save| save.absorption)),
        pearl_cd: Mutex::new(Instant::now() - Duration::from_secs(10)),
        seen: Mutex::new(HashSet::new()),
        chunks: Mutex::new(HashSet::new()),
        chunk_ack: Mutex::new(chunk_ack),
        gen_sources: Mutex::new(gen),
        xp: Mutex::new(saved_player.as_ref().map_or(0, |save| save.xp_total)),
        gm: Mutex::new(initial_gm),
        fall_from: Mutex::new(sy),
        food: Mutex::new(saved_player.as_ref().map_or(20, |save| save.food)),
        air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
        armor_pts: Mutex::new(0),
        sat: Mutex::new(saved_player.as_ref().map_or(5.0, |save| save.saturation)),
        walk_acc: Mutex::new(saved_player.as_ref().map_or(0.0, |save| save.exhaustion)),
        lastp: Mutex::new((sx, sy, sz)),
        spawn_pt: Mutex::new(saved_player.as_ref().and_then(|save| save.respawn.map(|point| {
            (point.position.0, point.position.1, point.position.2, point.angle)
        }))),
        dim: Mutex::new(saved_dim),
        view: Mutex::new(view),
        spawn_dim: Mutex::new(saved_player.as_ref().and_then(|save| save.respawn.map(|point| point.dim)).unwrap_or(saved_dim)),
        in_portal: Mutex::new(false),
        portal_at: Mutex::new(None),
        skin: Mutex::new(login_skin.clone()),
        held: Mutex::new(initial_held),
        effs: Mutex::new(saved_player.as_ref().map_or_else(Vec::new, |save| {
            save.effects.iter().map(|&(id, amp, left)| Eff { id, amp, left }).collect()
        })),
        lod_v1_enabled: AtomicBool::new(false),
        underground_revealed: AtomicBool::new(!initial_occluded),
        sleeping: Mutex::new(None),
        inv_state_id: AtomicI32::new(0),
    });
    // Published to the panic cleanup in `handle` before the client list sees
    // the player, so a panic anywhere below still unregisters this eid.
    live_eid.store(me.eid, Ordering::SeqCst);
    let mut retry_chunks = chunk_delivery.has_pending();
    me.enter_play_wire(compression, chunk_delivery);
    {
        let border = world_border().lock().unwrap();
        let board = scoreboard().lock().unwrap();
        let mut tracker = player_advancements().lock().unwrap();
        restore_player_advancements(&mut tracker, &me.name);
        {
            let mut w = me.sock.lock().unwrap();
            let now = border_now_ms();
            let mut snapshot = *border;
            snapshot.finish_lerp(now);
            let (id, body) = snapshot.initialize_packet(now);
            let _ = send_packet(&mut w, id, body);
            let _ = sync_scoreboard(&mut w, &board);
        }
        send_advancement_delta(&mut tracker, &me, true);
        player_mechanism_sessions().lock().unwrap().insert(
            me.eid,
            PlayerMechanismSession {
                xp_bar: saved_player.as_ref().map_or((0, 0.0), |save| (save.xp_level, save.xp_progress)),
                food_timer: 0,
                menus: Arc::new(LiveMenuConnection {
                    active: Mutex::new(None),
                    next_id: std::sync::atomic::AtomicU8::new(4),
                    enchantment_seed: Mutex::new(i32::from_be_bytes(me.uuid[..4].try_into().unwrap())),
                }),
            },
        );
    }
    if let Some(save) = saved_player.as_ref() {
        chests().lock().unwrap().insert(
            ender_container_key(&me.uuid),
            save.ender_chest.clone(),
        );
        for &(id, amp, left) in &save.effects {
            let _ = send_packet(&mut me.sock.lock().unwrap(), 0x76, entity_effect_pkt(me.eid, id, amp, left));
        }
    }
    // Java sees the login before the tick thread can snapshot this player.
    #[cfg(feature = "neoforge-events")]
    if java_compat::NEOFORGE.get().is_some() {
        let _ = neoforge_event(&me, java_compat::S1Event::PlayerLogin(me.eid, me.name.clone()));
    }
    {
        // Match bossbar command lock order so a joining member receives one Add.
        let bars = bossbars().lock().unwrap();
        let mut cls = clients().lock().unwrap();
        *me.chunks.lock().unwrap() = sent;
        for c in cls.iter() {
            // tab-list info is global (vanilla keeps it world-wide);
            // the ENTITY spawn is interest-gated — the sweep sends it
            // only once the other player enters visible range.
            let _ = send_packet(&mut me.sock.lock().unwrap(), 0x3e, player_info_add(c));
            let pi = player_info_add(&me);
            if let Ok(mut w) = c.sock.lock() {
                let _ = send_packet(&mut w, 0x3e, pi.clone());
            }
        }
        cls.push(me.clone());
        for (id, bar) in bars.iter().filter(|(_, bar)| bar.shown_to(&me)) {
            let _ = send_packet(&mut me.sock.lock().unwrap(), 0x0a, bossbar_pkt(id, 0, Some(bar)));
        }
    }
    // An operator change can happen during the initial chunk stream, before
    // the client is present in the online list used by /op and /reload.
    if login_admin != player_is_admin(&me.name) {
        update_client_commands(&me);
    }
    // A forwarded or session-verified skin is already on `me`; only run the
    // offline Mojang lookup when login left the player skinless.
    if login_skin.is_none() {
        kick_skin(name.clone(), &me);
    }
    // refresh armor points from restored inventory
    {
        let iv = me.inv.lock().unwrap();
        *me.armor_pts.lock().unwrap() = armor_pts_of(&iv);
    }
    send_attrs(&me);
    // echo restored inventory slots so the client sees them
    {
        let inv = me.inv.lock().unwrap();
        for (&slot, st) in inv.iter() {
            let mut p = vec![0u8];
            write_varint(&mut p, 0);
            p.extend_from_slice(&slot.to_be_bytes());
            p.extend_from_slice(&slot_wire(st));
            let _ = send_packet(&mut me.sock.lock().unwrap(), 0x15, p);
        }
    }
    {
        let ms = mobs().lock().unwrap();
        // mobs/drops stream via interest_tick (distance-gated)
        drop(ms);
        // drops/xp/mobs stream via interest_tick (distance-gated)
    }
    let me_cleanup = me.clone();
    // Opt-in minimap fair-play (`MC_SPINE_FAIR_PLAY` or `/megu policy`
    // `minimap_mode`, default off): the control code goes out once here, on
    // play join. Off by default — nothing is sent unless the operator armed
    // the flag or restricted the map.
    {
        let policy = map_fair_play_policy();
        if policy.fair_play {
            eprintln!(
                "[spine] {} map fair-play on: cave_map={} entity_radar={}",
                me.name, policy.allow_cave_map, policy.allow_entity_radar
            );
        }
        for chat in map_fair_play_join_chats(&policy) {
            let mut sock = me.sock.lock().unwrap_or_else(|e| e.into_inner());
            let _ = send_packet(&mut sock, 0x6c, chat);
        }
    }
    // ---- consume loop ----
    s.set_write_timeout(Some(Duration::from_secs(2))).ok();
    let mut keepalive = connection_limits::KeepaliveState::new(Instant::now());
    let mut next_keepalive_id = 1i64;
    // The instant the outstanding keepalive challenge went out, mirrored so a
    // matching reply can be turned into a round-trip measurement.
    let mut keepalive_sent_at: Option<Instant> = None;
    let mut held_slot: i16 = initial_held;
    // `EntityEquipment.setChanged` baseline: every consumed packet may touch
    // the menu (hotbar switch, armor drag, plugin write), so the loop head
    // diffs the live view and ships one sparse SetEquipment.
    let mut last_equipment = player_equipment(&me);

    let mut rx = RxBuf::new(compression);
    // Play-state packet inactivity is tracked separately from the keep-alive deadline.
    let mut last_rx = Instant::now();
    let mut chat_throttle = connection_limits::ChatThrottle::new(Instant::now());
    let mut read_timeout = None;
    let mut last_chunk_retry_tick = TICK_IDX.load(Ordering::Relaxed);
    'conn: loop {
        drop_player_items_on_death(&me);
        {
            let equipment_now = player_equipment(&me);
            if let Some(packet) =
                encode_set_equipment_changes(me.eid, &last_equipment, &equipment_now)
            {
                last_equipment = equipment_now;
                broadcast_seen(me.eid, |_c| (0x5b, packet.clone()));
            }
        }
        use connection_limits::KeepaliveAction;
        match keepalive.tick(Instant::now(), next_keepalive_id) {
            KeepaliveAction::Send(id) => {
                if send_packet(&mut me.sock.lock().unwrap(), 0x26, id.to_be_bytes().to_vec())
                    .is_err()
                {
                    break 'conn;
                }
                keepalive_sent_at = Some(Instant::now());
                next_keepalive_id = next_keepalive_id.wrapping_add(1);
            }
            KeepaliveAction::TimedOut => {
                play_disconnect(&mut me.sock.lock().unwrap(), "Timed out");
                break 'conn;
            }
            KeepaliveAction::Wait => {}
        }
        // ACKs and movement can wake delivery immediately. An eligible backlog
        // gets another chance on the next tick, even during continuous input.
        let tick = TICK_IDX.load(Ordering::Relaxed);
        if retry_chunks && tick != last_chunk_retry_tick {
            retry_chunks = pump_client_chunks(&me, false);
            last_chunk_retry_tick = tick;
        }
        let timeout = if retry_chunks {
            Duration::from_millis(TICK_MS)
        } else {
            Duration::from_secs(2)
        };
        if read_timeout != Some(timeout) {
            s.set_read_timeout(Some(timeout)).ok();
            read_timeout = Some(timeout);
        }
        // A client that stopped answering chunk batches is gone, even on an idle socket.
        if me.chunk_ack.lock().unwrap().timed_out(Instant::now()) {
            eprintln!(
                "[spine] {} disconnected: chunk batch acknowledgement timeout",
                me.name
            );
            break 'conn;
        }
        let (pid, b) = match rx.next(&mut s) {
            RxOut::Closed => break 'conn,
            RxOut::Idle => {
                let idle_limit = connection_limits::idle_timeout();
                if !idle_limit.is_zero() && last_rx.elapsed() > idle_limit {
                    eprintln!(
                        "[spine] {} disconnected: no packet for {:?}",
                        me.name, idle_limit
                    );
                    play_disconnect(&mut me.sock.lock().unwrap(), "Timed out");
                    break 'conn;
                }
                // A dimension change initiated by another connection can
                // create a backlog even while this connection is idle.
                let tick = TICK_IDX.load(Ordering::Relaxed);
                if tick != last_chunk_retry_tick {
                    retry_chunks = pump_client_chunks(&me, false);
                    last_chunk_retry_tick = tick;
                }
                continue;
            }
            RxOut::Frame(pid, b) => {
                last_rx = Instant::now();
                (pid, b)
            }
        };
        drop_player_items_on_death(&me);

        // vanilla refuses a movement update whose position is not finite or is
        // outside the world; letting it through carries the value into worldgen,
        // visibility and every float-to-int conversion behind them.
        if !ingress_guard::valid_movement_frame(pid, &b) {
            eprintln!("[spine] {} disconnected: malformed movement frame", me.name);
            break 'conn;
        }

        match pid {
            0x00 => {
                let mut payload = b.as_slice();
                if let Some(id) = read_protocol_i32(&mut payload).filter(|_| payload.is_empty()) {
                    me.teleport.lock().unwrap().confirm(id);
                    retry_chunks = pump_client_chunks(&me, false);
                }
            }
            0x18 => {
                if !keepalive.reply(Instant::now(), &b) {
                    play_disconnect(&mut me.sock.lock().unwrap(), "Timed out");
                    break 'conn;
                }
                // A reply that matched the pending challenge is a measured
                // round-trip: feed it into the OPS-2 lag-compensation envelope
                // so latency spikes do not read as movement cheats.
                if let Some(sent_at) = keepalive_sent_at.take() {
                    let rtt_ms =
                        sent_at.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
                    ac_note_player_rtt_ms(me.eid, rtt_ms);
                }
            }
            0x02 => {
                // Serverbound change difficulty: difficulty byte + locked flag.
                if b.len() == 2 && b[0] <= 3 && b[1] <= 1 && player_is_admin(&me.name) {
                    set_difficulty(b[0]);
                }
            }
            0x08 => {
                // Java 1.21.1 Chunk Batch Received: desired chunks per tick, four
                // big-endian f32 bytes. Answering it opens the next batch.
                let acked = {
                    let mut ack = me.chunk_ack.lock().unwrap();
                    ack.acknowledge(&b)
                };
                if let Err(why) = acked {
                    eprintln!("[spine] {} disconnected: {why}", me.name);
                    break 'conn;
                }
                retry_chunks = pump_client_chunks(&me, false);
            }
            0x0a => {
                let Some(requested) = client_info_view_distance(&b) else {
                    break 'conn;
                };
                let effective = requested.clamp(2, SERVER_VIEW_DISTANCE);
                if effective != view {
                    view = effective;
                    *me.view.lock().unwrap() = view;
                    send_view_distance(&mut me.sock.lock().unwrap(), view);
                    connection_limits::set_chunk_feedback_window(
                        &mut me.chunk_ack.lock().unwrap(),
                        window_batch(view),
                    );
                    retry_chunks = pump_client_chunks(&me, true);
                }
            }
            id if id == i64::from(brigadier_wire::SERVERBOUND_COMMAND_SUGGESTION) => {
                reply_to_command_suggestion(&me, &b);
            }
            0x1a | 0x1b => {
                // position / position_look: f64 x,y,z first
                if b.len() >= 24 {
                    let px = f64::from_be_bytes(b[..8].try_into().unwrap());
                    let py = f64::from_be_bytes(b[8..16].try_into().unwrap());
                    let pz = f64::from_be_bytes(b[16..24].try_into().unwrap());
                    if !me.teleport.lock().unwrap().movement_allowed() {
                        continue;
                    }
                    // Vanilla wakes a sleeping player when they move.
                    wake_if_sleeping(&me);
                    // Anti-cheat runs before any lock is taken: it reads attributes, effects
                    // and blocks around the player, and may pull the player back.
                    let before = *me.pos.lock().unwrap();
                    let claimed = if pid == 0x1b && b.len() >= 33 {
                        b[32] != 0
                    } else if pid == 0x1a && b.len() >= 25 {
                        b[24] != 0
                    } else {
                        before.5
                    };
                    let checked_gnd = match ac_check_move(
                        &me,
                        (before.0, before.1, before.2),
                        (px, py, pz),
                        claimed,
                    ) {
                        AcMoveVerdict::Accept { ground } => ground,
                        AcMoveVerdict::Setback => {
                            ac_setback(&me, before);
                            continue;
                        }
                    };
                    let teleport = me.teleport.lock().unwrap();
                    if !teleport.movement_allowed() {
                        continue;
                    }
                    let mut p = me.pos.lock().unwrap();
                    let (mut yaw, mut pitch, gnd) = (p.3, p.4, checked_gnd);
                    if pid == 0x1b && b.len() >= 33 {
                        yaw = f32::from_be_bytes(b[24..28].try_into().unwrap());
                        pitch = f32::from_be_bytes(b[28..32].try_into().unwrap());
                    }
                    let prev_gnd = p.5;
                    *p = (px, py, pz, yaw, pitch, gnd);
                    drop(p);
                    drop(teleport);
                    landing::player_move(&me, prev_gnd);
                    // P2-3-S4: standing in a portal resps the dimension switch. The guard
                    // lives on the player, so arriving in a destination portal does not
                    // immediately trigger the return trip.
                    if tick_portal(&me, view) {
                        continue;
                    }
                    // A vertical move inside the same chunk window still crosses the
                    // occlusion hysteresis (descending into a cave); the lane flip
                    // re-sends held chunks in the new lane. The window pump below
                    // re-runs the same check as a no-op when nothing changed.
                    if before.1 != py {
                        update_underground_lane(&me);
                    }
                    let window = ChunkWindow::at(px, pz, view);
                    let current = with_play_wire(&me.sock.lock().unwrap(), |play| {
                        play.chunk_delivery.window()
                    })
                    .flatten();
                    if current != Some(window) {
                        if let Ok(pending) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            pump_client_chunks(&me, true)
                        })) {
                            retry_chunks = pending;
                        }
                    }
                    broadcast_seen(me.eid, |_c| {
                        (0x70, entity_teleport(me.eid, px, py, pz, yaw, pitch, gnd))
                    });
                    if pid == 0x1b {
                        broadcast_seen(me.eid, |_c| (0x48, head_rot(me.eid, yaw)));
                    }
                }
            }
            0x1c => {
                // look: yaw,pitch only
                if b.len() >= 9 {
                    let yaw = f32::from_be_bytes(b[..4].try_into().unwrap());
                    let pitch = f32::from_be_bytes(b[4..8].try_into().unwrap());
                    // The same rule `valid_movement_frame` applies to the
                    // position packets: a non-finite angle must not reach `pos`,
                    // the rotation broadcasts, or the projectile math that turns
                    // yaw/pitch into velocities.
                    if !yaw.is_finite() || !pitch.is_finite() {
                        continue;
                    }
                    let teleport = me.teleport.lock().unwrap();
                    if !teleport.movement_allowed() {
                        continue;
                    }
                    let at = {
                        let p = me.pos.lock().unwrap();
                        (p.0, p.1, p.2)
                    };
                    let gnd = ac_effective_ground(&me, at, b[8] != 0);
                    let mut p = me.pos.lock().unwrap();
                    let prev_gnd = p.5;
                    p.3 = yaw;
                    p.4 = pitch;
                    p.5 = gnd;
                    let (px, py, pz, g) = (p.0, p.1, p.2, p.5);
                    drop(p);
                    drop(teleport);
                    landing::player_move(&me, prev_gnd);
                    broadcast_seen(me.eid, |_c| {
                        (0x70, entity_teleport(me.eid, px, py, pz, yaw, pitch, g))
                    });
                    broadcast_seen(me.eid, |_c| (0x48, head_rot(me.eid, yaw)));
                }
            }
            0x25 => {
                // entity_action: sneak/sprint start/stop -> entity flags+pose
                let mut sl = &b[..];
                let _eid = read_varint(&mut sl);
                if let Some(act) = read_varint(&mut sl) {
                    match act {
                        0 => *me.sneaking.lock().unwrap() = true,
                        1 => *me.sneaking.lock().unwrap() = false,
                        2 => {
                            wake_if_sleeping(&me);
                        }
                        3 => *me.sprinting.lock().unwrap() = true,
                        4 => *me.sprinting.lock().unwrap() = false,
                        _ => {}
                    }
                    if (0..=1).contains(&act) || (3..=4).contains(&act) {
                        let sneak = *me.sneaking.lock().unwrap();
                        let sprint = *me.sprinting.lock().unwrap();
                        let glide = *me.fall_fly.lock().unwrap();
                        // flags: sneak=0x02 sprint=0x08 fall_flying=0x80 ; pose: sneak=5 fall_fly=1 else standing=0
                        let mut p = Vec::new();
                        write_varint(&mut p, me.eid as i64);
                        p.push(0);
                        p.push(0);
                        p.push(((sneak as u8) * 2) | ((sprint as u8) * 8) | ((glide as u8) * 0x80));
                        p.push(6);
                        write_varint(&mut p, 21);
                        write_varint(
                            &mut p,
                            if glide {
                                1
                            } else if sneak {
                                5
                            } else {
                                0
                            },
                        );
                        p.push(0xff);
                        broadcast_seen(me.eid, |_c| (0x58, p.clone()));
                    }
                }
            }
            0x2f => {
                // held_item_slot: i16 — `EntityEquipment.setChanged` for the
                // new main hand is emitted by the loop-head equipment diff.
                if b.len() >= 2 {
                    held_slot = i16::from_be_bytes(b[..2].try_into().unwrap()).clamp(0, 8);
                    *me.held.lock().unwrap() = held_slot;
                }
            }
            0x20 => {
                // pick_item: the server ray-traces the block under the
                // crosshair itself and runs `Inventory.pick` on its item —
                // the one-byte includeData flag needs no parse.
                pick_item(&me, &mut held_slot);
            }
            0x1d => {
                // flying: onGround only
                if !b.is_empty() {
                    let teleport = me.teleport.lock().unwrap();
                    if !teleport.movement_allowed() {
                        continue;
                    }
                    let at = {
                        let p = me.pos.lock().unwrap();
                        (p.0, p.1, p.2)
                    };
                    let gnd = ac_effective_ground(&me, at, b[0] != 0);
                    let mut p = me.pos.lock().unwrap();
                    let prev_gnd = p.5;
                    p.5 = gnd;
                    drop(p);
                    drop(teleport);
                    landing::player_move(&me, prev_gnd);
                }
            }
            0x1e => {
                // ServerboundMoveVehiclePacket: the riding client drives the
                // vehicle. `step_live_vehicle` runs the vanilla speed gate and
                // credits the carry to anti-cheat, so riding never trips the
                // mover checks.
                if let Some(pkt) = decode_move_vehicle(&b) {
                    step_live_vehicle(&me, &pkt);
                }
            }
            0x1f => {
                // ServerboundPaddleBoatPacket: remembered per vehicle for the
                // boat-paddle animation state; it moves no world state itself.
                if let Some(paddle) = decode_paddle_boat(&b) {
                    if let Some(vehicle) = vehicle_of(me.eid) {
                        mounts().lock().unwrap().paddling.insert(vehicle, paddle);
                    }
                }
            }
            0x26 => {
                // ServerboundPlayerInputPacket: the shift bit dismounts the
                // rider (`Entity.stopRiding`), updating the vehicle's
                // SetPassengers for every tracker. The rest of the input is
                // the driver's steering for the live vehicle physics tick.
                if let Some(input) = decode_player_input(&b) {
                    if let Some(vehicle) = vehicle_of(me.eid) {
                        if input.unmount {
                            mounts().lock().unwrap_or_else(|e| e.into_inner()).steering.remove(&vehicle);
                        } else {
                            mounts().lock().unwrap_or_else(|e| e.into_inner()).steering.insert(vehicle, input);
                        }
                    }
                    if input.unmount {
                        player_dismount(&me, true);
                    }
                }
            }
            0x24 => {
                // block_dig
                let mut sl = &b[..];
                let status = read_varint(&mut sl).unwrap_or(-1);
                let (x, y, z) = if sl.len() >= 8 {
                    read_pos(&sl[..8])
                } else {
                    (0, 0, 0)
                };
                let seq = if sl.len() >= 9 {
                    let mut s2 = &sl[9..];
                    read_varint(&mut s2).unwrap_or(0)
                } else {
                    0
                };
                if status == 5 {
                    // finish using item (bow release / lower shield)
                    *me.blocking.lock().unwrap() = false;
                    let eat0 = me.eat_start.lock().unwrap().take();
                    if let Some(t0) = eat0 {
                        // vanilla: release after 32 ticks consumes; earlier release aborts
                        if t0.elapsed().as_secs_f32() >= 1.4 {
                            air_use_item(&me, held_slot, seq, None, true);
                            continue;
                        }
                    }
                    let draw_t = me
                        .bow_draw
                        .lock()
                        .unwrap()
                        .take()
                        .map(|st| st.elapsed().as_secs_f32())
                        .unwrap_or(0.0);
                    if draw_t > 0.0 {
                        let arrow_slot = me
                            .inv
                            .lock()
                            .unwrap()
                            .iter()
                            .find(|(_, (i, c, _))| *i == 802 && *c > 0)
                            .map(|(&sl, _)| sl);
                        let (hit, _) = {
                            let inv = me.inv.lock().unwrap();
                            (inv.get(&(36 + held_slot)).map(|v| v.0).unwrap_or(0), 0)
                        };
                        let charge_t = (draw_t / 1.19).min(1.0);
                        let qc = {
                            let i2 = me.inv.lock().unwrap();
                            enchant_level(
                                &i2.get(&(36 + held_slot))
                                    .map(|v| v.2.clone())
                                    .unwrap_or_default(),
                                29,
                            )
                        };
                        let can_fire = if hit == 801 {
                            charge_t >= 0.1
                        } else {
                            draw_t >= 1.25 - 0.25 * (qc as f32).min(5.0)
                        };
                        if let (true, true, Some(asl)) =
                            ((hit == 801 || hit == 1192), can_fire, arrow_slot)
                        {
                            let pow_f = (charge_t * charge_t + 2.0 * charge_t) / 3.0;
                            let inf = {
                                let i2 = me.inv.lock().unwrap();
                                enchant_level(
                                    &i2.get(&(36 + held_slot))
                                        .map(|v| v.2.clone())
                                        .unwrap_or_default(),
                                    16,
                                ) > 0
                            };
                            if !inf {
                                let mut inv = me.inv.lock().unwrap();
                                let n = inv[&asl].1 - 1;
                                if n == 0 {
                                    inv.remove(&asl);
                                } else {
                                    inv.get_mut(&asl).unwrap().1 = n;
                                }
                            }
                            let (x, y, z, yaw, pitch, _g) = *me.pos.lock().unwrap();
                            let yr = (yaw as f64).to_radians();
                            let pr = (pitch as f64).to_radians();
                            // bow enchants: power scales arrow dmg, flame ignites, punch knocks
                            let held_c = {
                                let i2 = me.inv.lock().unwrap();
                                i2.get(&(36 + held_slot))
                                    .map(|v| v.2.clone())
                                    .unwrap_or_default()
                            };
                            let pw = enchant_level(&held_c, 25);
                            let fl = enchant_level(&held_c, 12);
                            let pu = enchant_level(&held_c, 28);
                            let pi = if hit == 1192 {
                                enchant_level(&held_c, 24)
                            } else {
                                0
                            };
                            let a = Arrow {
                                eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                                owner: me.eid,
                                x,
                                y: y + 1.5,
                                z,
                                dx: -yr.sin() * pr.cos() * 2.5 * pow_f as f64,
                                dy: -pr.sin() * 2.5 * pow_f as f64,
                                dz: yr.cos() * pr.cos() * 2.5 * pow_f as f64,
                                ttl: server_ticks(ARROW_TTL_GT) as i32,
                                dmg: 6.0 * pow_f * (1.0 + 0.25 * pw as f32),
                                fire: if fl > 0 { server_ticks(100) as i32 } else { 0 },
                                punch: pu as i32,
                                kind: 0,
                                loy: 0,
                                ret_item: 0,
                                ret_comps: Vec::new(),
                                pierce: pi as i32,
                                last_hit: -1,
                                stuck: false,
                            };
                            let (admg, afire, apunch, apierce) = (a.dmg, a.fire, a.punch, a.pierce);
                            arrows().lock().unwrap().push(a);
                            // multishot (crossbow): +2 arrows spread +-10deg
                            if hit == 1192 && enchant_level(&held_c, 23) > 0 {
                                for off in [-0.1745f64, 0.1745] {
                                    let yr2 = yr + off;
                                    arrows().lock().unwrap().push(Arrow {
                                        eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                                        owner: me.eid,
                                        x,
                                        y: y + 1.5,
                                        z,
                                        dx: -yr2.sin() * pr.cos() * 2.5 * pow_f as f64,
                                        dy: -pr.sin() * 2.5 * pow_f as f64,
                                        dz: yr2.cos() * pr.cos() * 2.5 * pow_f as f64,
                                        ttl: server_ticks(ARROW_TTL_GT) as i32,
                                        dmg: admg,
                                        fire: afire,
                                        punch: apunch,
                                        kind: 0,
                                        loy: 0,
                                        ret_item: 0,
                                        ret_comps: Vec::new(),
                                        pierce: apierce,
                                        last_hit: -1,
                                        stuck: false,
                                    });
                                }
                            }
                            // spawn streams via interest_tick
                            // vanilla: firing a bow/crossbow wears it
                            if damage_held(&me) {
                                let snd = sound_pkt_named("entity.item.break", x, y + 1.0, z);
                                broadcast_near(
                                    *me.dim.lock().unwrap(),
                                    x,
                                    y + 1.0,
                                    z,
                                    -1,
                                    |_c| (0x68, snd.clone()),
                                );
                            }
                            {
                                let inv = me.inv.lock().unwrap().get(&asl).cloned();
                                let mut p = vec![4u8];
                                write_varint(&mut p, 0);
                                p.extend_from_slice(&(36i16 + held_slot).to_be_bytes());
                                let held_now = {
                                    let i2 = me.inv.lock().unwrap();
                                    i2.get(&(36 + held_slot))
                                        .cloned()
                                        .unwrap_or((0, 0, Vec::new()))
                                };
                                p.extend_from_slice(&slot_wire(&held_now));
                                let _ = send_packet(&mut me.sock.lock().unwrap(), 0x15, p);
                                let mut p2 = vec![4u8];
                                write_varint(&mut p2, 0);
                                p2.extend_from_slice(&asl.to_be_bytes());
                                let iv = inv.unwrap_or((0, 0, Vec::new()));
                                p2.extend_from_slice(&slot_wire(&iv));
                                let _ = send_packet(&mut me.sock.lock().unwrap(), 0x15, p2);
                            }
                        }
                        if hit == name_to_item("trident").unwrap_or(-9) && draw_t >= 0.1 {
                            // trident throw
                            let held_st = {
                                let i2 = me.inv.lock().unwrap();
                                i2.get(&(36 + held_slot))
                                    .cloned()
                                    .unwrap_or((0, 0, Vec::new()))
                            };
                            let loy = enchant_level(&held_st.2, 19);
                            let imp = enchant_level(&held_st.2, 15) as f32 * 2.5;
                            let chan = enchant_level(&held_st.2, 5);
                            // survival consumes the thrown trident; it lands as a
                            // pickupable drop (or returns at owner's feet w/ loyalty)
                            if *me.gm.lock().unwrap() != 1 {
                                let mut inv = me.inv.lock().unwrap();
                                let hs = 36 + held_slot;
                                if let Some(st) = inv.get_mut(&hs) {
                                    if st.1 <= 1 {
                                        inv.remove(&hs);
                                    } else {
                                        st.1 -= 1;
                                    }
                                }
                            }
                            let (x, y, z, yaw, pitch, _g) = *me.pos.lock().unwrap();
                            let yr = (yaw as f64).to_radians();
                            let pr = (pitch as f64).to_radians();
                            arrows().lock().unwrap().push(Arrow {
                                eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                                owner: me.eid,
                                x,
                                y: y + 1.5,
                                z,
                                dx: -yr.sin() * pr.cos() * 2.2,
                                dy: -pr.sin() * 2.2,
                                dz: yr.cos() * pr.cos() * 2.2,
                                ttl: server_ticks(ARROW_TTL_GT) as i32,
                                dmg: 8.0 + imp,
                                fire: 0,
                                punch: chan as i32,
                                kind: 1,
                                loy,
                                ret_item: held_st.0,
                                ret_comps: held_st.2.clone(),
                                pierce: 0,
                                last_hit: -1,
                                stuck: false,
                            });
                            held_slot_resync(&me);
                        }
                    }
                }
                if status == 6 {
                    // swap_item_in_hand: held <-> offhand (F key)
                    let hs = 36 + held_slot;
                    let mut inv = me.inv.lock().unwrap();
                    let a = inv.remove(&hs).unwrap_or((0, 0, Vec::new()));
                    let b = inv.remove(&45i16).unwrap_or((0, 0, Vec::new()));
                    if b.1 > 0 {
                        inv.insert(hs, b.clone());
                    }
                    if a.1 > 0 {
                        inv.insert(45, a.clone());
                    }
                    drop(inv);
                    resync_slot(&me, 0, hs, &b);
                    resync_slot(&me, 0, 45, &a);
                }
                if status == 3 || status == 4 {
                    // DROP_ALL_ITEMS (ctrl+Q) / DROP_ITEM (Q)
                    {
                        // vanilla: Q-drop works in creative too
                        let hs = 36 + held_slot;
                        let it = {
                            let mut inv = me.inv.lock().unwrap();
                            let take = inv.get(&hs).map(|st| if status == 3 { st.1 } else { 1 });
                            take.and_then(|t| {
                                inv.get(&hs).map(|st| (st.0, t, st.1 <= t, st.2.clone()))
                            })
                            .map(|(item, t, clear, comps)| {
                                if clear {
                                    inv.remove(&hs);
                                } else {
                                    inv.get_mut(&hs).unwrap().1 -= t;
                                }
                                (item, t, comps)
                            })
                        };
                        if let Some((item, cnt, comps)) = it {
                            let (px, py, pz, ..) = {
                                let p = me.pos.lock().unwrap();
                                *p
                            };
                            drops().lock().unwrap().push(Drop {
                                eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                                x: px,
                                y: py + 1.3,
                                z: pz,
                                item,
                                cnt,
                                comps,
                                age: 0,
                                target: None,
                                thrower: Some(me.uuid),
                            });
                            {
                                let mut p = vec![0u8];
                                write_varint(&mut p, 0);
                                p.extend_from_slice(&hs.to_be_bytes());
                                let now = me.inv.lock().unwrap().get(&hs).cloned().unwrap_or((
                                    0,
                                    0,
                                    Vec::new(),
                                ));
                                p.extend_from_slice(&slot_wire(&now));
                                if let Ok(mut w) = me.sock.lock() {
                                    let _ = send_packet(&mut w, 0x15, p);
                                }
                            }
                        }
                    }
                }
                if status == 1 {
                    *me.dig.lock().unwrap() = None;
                } // aborted dig
                if status == 0 || status == 2 {
                    // vanilla block_interaction_range: 4.5 survival / 5.0 creative
                    let (px, py, pz, _, _, on_ground) = *me.pos.lock().unwrap();
                    let game_mode = *me.gm.lock().unwrap();
                    let gm_creative = game_mode == 1;
                    let reach = if gm_creative { 5.6f64 } else { 4.9 };
                    let (ddx, ddy, ddz) = (
                        x as f64 + 0.5 - px,
                        y as f64 + 0.5 - (py + 1.62),
                        z as f64 + 0.5 - pz,
                    );
                    let in_range = ddx * ddx + ddy * ddy + ddz * ddz <= reach * reach;
                    let dim = *me.dim.lock().unwrap();
                    let old = block_at_dim(dim, x, y, z);
                    let held = {
                        let inv = me.inv.lock().unwrap();
                        inv.get(&(36 + held_slot))
                            .cloned()
                            .unwrap_or((0, 0, Vec::new()))
                    };
                    let req = if in_range {
                        let (haste, fatigue) = {
                            let effects = me.effs.lock().unwrap();
                            (
                                effects.iter().find(|effect| effect.id == 2).map(|effect| effect.amp),
                                effects.iter().find(|effect| effect.id == 3).map(|effect| effect.amp),
                            )
                        };
                        let underwater = block_registry::breg().is_some_and(|registry| {
                            let (bx, by, bz) = (
                                px.floor() as i32,
                                (py + 1.62).floor() as i32,
                                pz.floor() as i32,
                            );
                            registry.has_water(block_at_dim(dim, bx, by, bz))
                        });
                        let aqua_affinity = me.inv.lock().unwrap().get(&5)
                            .is_some_and(|helmet| enchant_level(&helmet.2, 0) > 0);
                        let base_speed = tool_dig_speed(&held, old as i64);
                        let modified_speed = mining_speed_with_modifiers(
                            base_speed,
                            false,
                            0,
                            haste,
                            fatigue,
                            underwater && !aqua_affinity,
                            on_ground,
                        );
                        dig_seconds(old as i64, &held)
                            .map(|seconds| seconds * base_speed / modified_speed.max(f32::MIN_POSITIVE))
                    } else {
                        None
                    };
                    // vanilla dig timing: survival needs the destroy duration for
                    // the block+tool; finish packets arriving early are rejected.
                    let mut break_now = false;
                    // Adventure `mayDestroy`: only a held stack whose
                    // can_break predicates match the block may dig it.
                    let may_destroy = game_mode != 2
                        || block_registry::breg().is_some_and(|reg| {
                            adventure_stack_allows(reg, old, &held, COMP_CAN_BREAK)
                        });
                    if in_range && old != 0 && may_destroy {
                        if (gm_creative && status == 0) || req == Some(0.0) {
                            break_now = true;
                        } else if let Some(t) = req {
                            if status == 0 {
                                *me.dig.lock().unwrap() = Some(PendingDig {
                                    pos: (dim, x, y, z),
                                    state: old,
                                    started: Instant::now(),
                                    seconds: t,
                                });
                            } else {
                                let t0 = me.dig.lock().unwrap().and_then(|d| {
                                    if (d.pos.1, d.pos.2, d.pos.3) == (x, y, z) {
                                        Some(d.started)
                                    } else {
                                        None
                                    }
                                });
                                if t0
                                    .map(|t0| t0.elapsed().as_secs_f32() >= t * 0.7)
                                    .unwrap_or(false)
                                {
                                    break_now = true;
                                }
                            }
                        }
                    }
                    if break_now {
                        *me.dig.lock().unwrap() = None;
                    }
                    if !break_now && status == 2 && old != 0 {
                        note_block_update_dim(dim, x, y, z, old); // rejected: resync truth
                    }
                    if break_now {
                        finish_block_break(&me, dim, x, y, z, old);
                    }
                }
                let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
            }
            0x38 => {
                // block_place
                let mut sl = &b[..];
                let _hand = read_varint(&mut sl);
                let (x, y, z) = if sl.len() >= 8 {
                    read_pos(&sl[..8])
                } else {
                    (0, 0, 0)
                };
                let mut s2 = &sl[8.min(sl.len())..];
                let face = read_varint(&mut s2).unwrap_or(1);
                let _ = &s2[..13.min(s2.len())]; // cursor+inside
                let seq = if s2.len() >= 13 {
                    let mut s3 = &s2[13..];
                    read_varint(&mut s3).unwrap_or(0)
                } else {
                    0
                };
                // air-use form (1.21+): use_item arrives as block_place
                // {-1,255,-1}, face -1 — dispatch to the shared item-use path
                if x == -1 && y == 255 && z == -1 && face == -1 {
                    air_use_item(&me, held_slot, seq, _hand, false);
                    continue;
                }
                // vanilla block_interaction_range: 4.5 survival / 5.0 creative
                // (clicked block center vs eye) — gates placement AND
                // block interactions (containers/signs/buckets all below).
                {
                    let (px, py, pz, ..) = *me.pos.lock().unwrap();
                    let reach = if *me.gm.lock().unwrap() == 1 {
                        5.6f64
                    } else {
                        4.9
                    };
                    let (rx, ry, rz) = (
                        x as f64 + 0.5 - px,
                        y as f64 + 0.5 - (py + 1.62),
                        z as f64 + 0.5 - pz,
                    );
                    if rx * rx + ry * ry + rz * rz > reach * reach {
                        let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                        continue;
                    }
                }
                let (dx, dy, dz) = FACE_OFF[(face.clamp(0, 5)) as usize];
                let (tx, ty, tz) = (x + dx, y + dy, z + dz);
                let dim = *me.dim.lock().unwrap();
                let clicked = block_at_dim(dim, x, y, z);
                let place = |pos: BlockPos, state| {
                    if *me.gm.lock().unwrap() == 2 {
                        // Adventure `mayPlace`: the held stack's can_place_on
                        // predicates must match the clicked block.
                        let held = me
                            .inv
                            .lock()
                            .unwrap()
                            .get(&(36 + held_slot))
                            .cloned()
                            .unwrap_or((0, 0, Vec::new()));
                        let allowed = block_registry::breg().is_some_and(|reg| {
                            adventure_stack_allows(reg, clicked, &held, COMP_CAN_PLACE_ON)
                        });
                        if !allowed {
                            return false;
                        }
                    }
                    if !ac_check_place(&me, dim, (x, y, z), clicked, pos) {
                        ac_resync_block(&me, dim, pos);
                        return false;
                    }
                    #[cfg(feature = "neoforge-events")]
                    if java_compat::NEOFORGE.get().is_some() {
                        let (x, y, z) = pos;
                        let old = block_at_dim(dim, x, y, z);
                        if !neoforge_place_allowed(&me, (dim, x, y, z), old, state, clicked) {
                            return false;
                        }
                        if !set_block_if_dim(dim, x, y, z, state, Some(old)) {
                            neoforge_resync_block(&me, dim, pos);
                            return false;
                        }
                        note_block_update_dim(dim, x, y, z, block_at_dim(dim, x, y, z));
                        return true;
                    }
                    place_block_dim(dim, pos.0, pos.1, pos.2, state);
                    true
                };
                let sneaking = *me.sneaking.lock().unwrap();
                if !sneaking
                    && crate::block_registry::breg().is_some_and(|reg| {
                        live_block_entity_id(reg.name_of(clicked)) == Some("minecraft:beacon")
                    })
                {
                    // vanilla BeaconBlock.useWithoutItem opens the beacon screen.
                    // Until a live beacon window exists the held stack stands in
                    // for its payment slot; the anchor is what a real
                    // `set_beacon_effect` would resolve against.
                    beacon_menu_anchors()
                        .lock()
                        .unwrap()
                        .insert(me.eid, (dim, x, y, z));
                    with_save_guard(|| {
                        if let Some(reg) = crate::block_registry::breg() {
                            live_block_entities()
                                .lock()
                                .unwrap()
                                .at(reg, (dim, x, y, z), clicked);
                        }
                    });
                    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                    continue;
                }
                if !sneaking
                    && (open_live_block_menu(&me, (dim, x, y, z), clicked)
                        || use_redstone_component(&me, (dim, x, y, z), clicked))
                {
                    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                    continue;
                }
                // The older right-click handlers use Overworld-only block state.
                // In another dimension, route the newly supported items before them.
                if dim != DimId::Overworld {
                    let held = me.inv.lock().unwrap().get(&(36 + held_slot)).map(|s| s.0);
                    if let Some(item) = held {
                        if try_live_block_placement(
                            &me,
                            (dim, tx, ty, tz),
                            face.clamp(0, 5) as usize,
                            held_slot,
                            item,
                            clicked,
                        ) {
                            let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                            continue;
                        }
                    }
                }
                // existing sign: right-click reopens the editor (front text)
                if (4302..=4589).contains(&block_at_dim(dim, x, y, z))
                    || (19276..=19339).contains(&block_at_dim(dim, x, y, z))
                {
                    let mut op = Vec::new();
                    write_pos(&mut op, x, y, z);
                    op.push(1);
                    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x34, op);
                    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                    continue;
                }
                // chest: open container instead of placing
                if let Some(kind) = container_kind(block_at_dim(dim, x, y, z)) {
                    // The window is `DoubleBlockCombiner` order, not click order: the
                    // right half owns slots 0..=26 whichever half was right-clicked.
                    let window = chest_window_for_player(dim, x, y, z);
                    // `ChestBlock.getMenuProvider` (`dhb.b`): refuses single or double
                    // chest if either half has a redstone conductor or sitting cat above.
                    // `BarrelBlock` (`dfe.a`) does not check `isBlockedChestByBlock`.
                    if (kind == "Chest" && chest_window_blocked_dim(dim, window))
                        || (me.is_spectator() && window_has_pending_loot(window))
                    {
                        let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                        continue;
                    }
                    let audience = client_snapshot();
                    let mut halves = chest_window_halves_dim(window);
                    // A second container opening on the same client closes the first,
                    // which drops the first one's opener count too.
                    if let Some((previous, prev_anchor)) = client_open_window_and_anchor(&me) {
                        for half in client_window_halves_dim(previous, prev_anchor, dim) {
                            if !halves.contains(&half) {
                                halves.push(half);
                            }
                        }
                    }
                    let before = chest_opener_snapshot_dim(&audience, &halves);
                    close_live_menu(&me, false, true);
                    let inv = me.inv.lock().unwrap().clone();
                    open_chest_for_client(&me, window, None, &inv, kind);
                    // `ContainerOpenersCounter.incrementOpeners` -> openerCountChanged
                    sync_chest_openers_dim(&audience, &before);
                    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                    continue;
                }
                // ender chest: per-player shared storage (synthetic key, not world pos)
                if (7513..=7520).contains(&block_at_dim(dim, x, y, z)) {
                    // `EnderChestBlock.useWithoutItem` (`dip.a`): refuses when the cell
                    // above `isRedstoneConductor`.
                    if is_blocked_chest_by_block_dim(dim, (x, y, z)) {
                        let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                        continue;
                    }
                    let audience = client_snapshot();
                    let mut halves = vec![(dim, x, y, z)];
                    if let Some((previous, prev_anchor)) = client_open_window_and_anchor(&me) {
                        for half in client_window_halves_dim(previous, prev_anchor, dim) {
                            if !halves.contains(&half) {
                                halves.push(half);
                            }
                        }
                    }
                    let before = chest_opener_snapshot_dim(&audience, &halves);
                    let key = ender_container_key(&me.uuid);
                    close_live_menu(&me, false, true);
                    let inv = me.inv.lock().unwrap().clone();
                    open_chest_for_client(&me, (key, None), Some((x, y, z)), &inv, "Ender Chest");
                    sync_chest_openers_dim(&audience, &before);
                    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                    continue;
                }
                // flint & steel on TNT -> ignite
                if (2094..=2095).contains(&block_at_dim(dim, x, y, z))
                    && me
                        .inv
                        .lock()
                        .unwrap()
                        .get(&(36 + held_slot))
                        .map(|v| v.0)
                        .unwrap_or(0)
                        == 798
                {
                    set_block_dim(dim, x, y, z, 0);
                    note_block_update_dim(dim, x, y, z, 0);
                    let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
                    primed().lock().unwrap().push(Primed {
                        eid,
                        x: x as f64 + 0.5,
                        y: y as f64 + 0.5,
                        z: z as f64 + 0.5,
                        left: server_ticks(TNT_FUSE_GT) as i32,
                    });
                    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                    continue;
                }
                if block_at_dim(dim, x, y, z) == name_to_state("crafting_table").unwrap_or(u32::MAX) {
                    close_live_menu(&me, false, true);
                    *me.open_table.lock().unwrap() = true;
                    let mut p = Vec::new();
                    write_varint(&mut p, 3); // windowId 3
                    write_varint(&mut p, 12); // crafting menu
                    p.extend_from_slice(&chat_nbt("Crafting"));
                    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x33, p);
                    let mut q = vec![3u8];
                    write_varint(&mut q, 0);
                    write_varint(&mut q, 46);
                    for _ in 0..46 {
                        q.extend_from_slice(&slot_wire(&(0, 0, Vec::new())));
                    }
                    q.extend_from_slice(&slot_wire(&(0, 0, Vec::new())));
                    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x13, q);
                    // now fill: grid empty, inv slots
                    let inv = me.inv.lock().unwrap().clone();
                    let mut r = vec![3u8];
                    write_varint(&mut r, 0);
                    write_varint(&mut r, 46);
                    for _ in 0..10 {
                        r.extend_from_slice(&slot_wire(&(0, 0, Vec::new())));
                    }
                    for wsl in 10..46i16 {
                        let (it, c, _cc) =
                            inv.get(&(wsl - 1)).cloned().unwrap_or((0, 0, Vec::new()));
                        r.extend_from_slice(&slot_wire(&(it, c, _cc)));
                    }
                    r.extend_from_slice(&slot_wire(&(0, 0, Vec::new())));
                    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x13, r);
                    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                    continue;
                }
                if block_at_dim(dim, x, y, z) == name_to_state("furnace").unwrap_or(u32::MAX) {
                    close_live_menu(&me, false, true);
                    *me.open_furnace.lock().unwrap() = Some((dim, x, y, z));
                    let inv = me.inv.lock().unwrap().clone();
                    let slots = furnace_slot_snapshot_dim(dim, (x, y, z));
                    open_furnace_window(&mut me.sock.lock().unwrap(), &inv, &slots);
                    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                    continue;
                }
                // buckets: empty fills from source; filled places liquid
                let held_b = {
                    let inv = me.inv.lock().unwrap();
                    inv.get(&(36 + held_slot)).cloned()
                };
                if let Some((item, cnt, _hc)) = held_b {
                    let src = block_at_dim(dim, x, y, z);
                    if item == 908 && (src == 80 || src == 96) {
                        let fi = if src == 80 { 909 } else { 910 };
                        set_block_dim(dim, x, y, z, 0);
                        note_block_update_dim(dim, x, y, z, 0);
                        let mut inv = me.inv.lock().unwrap();
                        if cnt > 1 {
                            inv.insert(36 + held_slot, (908, cnt - 1, _hc));
                            let free = (9..=44).find(|sl| !inv.contains_key(sl)).unwrap_or(9);
                            inv.insert(free, (fi, 1, Vec::new()));
                        } else {
                            inv.insert(36 + held_slot, (fi, 1, Vec::new()));
                        }
                        let (i2, c2, _cc) =
                            inv.get(&(36 + held_slot))
                                .cloned()
                                .unwrap_or((0, 0, Vec::new()));
                        drop(inv);
                        let mut p = vec![0u8];
                        write_varint(&mut p, 0);
                        p.extend_from_slice(&(36 + held_slot).to_be_bytes());
                        p.extend_from_slice(&slot_wire(&(i2, c2, _cc)));
                        let _ = send_packet(&mut me.sock.lock().unwrap(), 0x15, p);
                        let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                        continue;
                    }
                    let replaceable = matches!(
                        block_at_dim(dim, tx, ty, tz),
                        0 | 2005 | 2006 | 2007 | 2075 | 2077 | 10756 | 5772..=5779
                    );
                    if (item == 909 || item == 910) && replaceable {
                        let st = if item == 909 { 80 } else { 96 };
                        if !place((tx, ty, tz), st) {
                            let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                            continue;
                        }
                        if *me.gm.lock().unwrap() != 1 {
                            let mut inv = me.inv.lock().unwrap();
                            inv.insert(36 + held_slot, (908, 1, Vec::new()));
                            drop(inv);
                            let mut p = vec![0u8];
                            write_varint(&mut p, 0);
                            p.extend_from_slice(&(36 + held_slot).to_be_bytes());
                            p.extend_from_slice(&slot_wire(&(908, 1, Vec::new())));
                            let _ = send_packet(&mut me.sock.lock().unwrap(), 0x15, p);
                        }
                        let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                        continue;
                    }
                }
                // bed: right-click at night skips to morning
                if block_registry::breg().is_some_and(|registry| {
                    let name = registry.name_of(clicked);
                    name.starts_with("minecraft:") && name.ends_with("_bed")
                }) {
                    let wt = WORLD_TIME.load(Ordering::Relaxed) % 24000;
                    // Vanilla sleeps at night or during a thunderstorm, and
                    // only in dimensions whose spec allows beds to work.
                    let sleep_ok = (13000..=23500).contains(&wt)
                        || THUNDERING.load(Ordering::Relaxed) > 0;
                    // Vanilla dims hardcode this (Overworld yes, Nether/End
                    // explode); custom dims read it from their spec.
                    let bed_works = dim == DimId::Overworld
                        || crate::worldgen::dimension_spec(dim)
                            .is_some_and(|spec| spec.bed_works);
                    if bed_works && sleep_ok {
                        let angle = me.pos.lock().unwrap().3;
                        let position = (x as f64 + 0.5, y as f64 + 1.0, z as f64 + 0.5);
                        {
                            let mut spawn_dim = me.spawn_dim.lock().unwrap();
                            *me.spawn_pt.lock().unwrap() =
                                Some((position.0, position.1, position.2, angle));
                            *spawn_dim = dim;
                        }
                        let inventory = me.inv.lock().unwrap().clone();
                        let mut snapshot = PlayerSave::capture(&me, client_dim_pos(&me), &inventory);
                        snapshot.respawn = Some(SpawnTarget {
                            kind: SpawnTargetKind::Bed,
                            dim,
                            position,
                            angle,
                            forced: false,
                        });
                        saved_players().lock().unwrap().insert(me.uuid, snapshot);
                        let mut sp = Vec::new();
                        write_pos(&mut sp, x, y, z);
                        sp.extend_from_slice(&angle.to_be_bytes());
                        let _ = send_packet(&mut me.sock.lock().unwrap(), 0x56, sp);
                        // Vanilla `startSleepInBed`: the player lies down and
                        // `sleep_tick` skips the night once enough Overworld
                        // players have slept 100 ticks
                        // (playersSleepingPercentage).
                        enter_bed(&me, x, y, z);
                    }
                    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                    continue;
                }
                // door toggle: flip open bit on both halves
                if let Some(base) = door_base(block_at_dim(dim, x, y, z)) {
                    for dy in [-1i32, 0, 1] {
                        let st = block_at_dim(dim, x, y + dy, z);
                        if door_base(st) == Some(base) {
                            set_block_dim(dim, x, y + dy, z, st ^ 2);
                            note_block_update_dim(dim, x, y + dy, z, st ^ 2);
                        }
                    }
                    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                    continue;
                }
                // fence gate toggle (open bit = stride 2)
                if gate_base(block_at_dim(dim, x, y, z)).is_some() {
                    let st = block_at_dim(dim, x, y, z);
                    set_block_dim(dim, x, y, z, st ^ 2);
                    note_block_update_dim(dim, x, y, z, st ^ 2);
                    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                    continue;
                }
                if trapdoor_state(block_at_dim(dim, x, y, z)) {
                    try_toggle_trapdoor(dim, x, y, z);
                    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                    continue;
                }
                // hotbar slots are window slots 36..=44
                let held = {
                    let inv = me.inv.lock().unwrap();
                    inv.get(&(36 + held_slot)).cloned()
                };
                // bone meal on crops: +2..4 stages, consume 1
                if let Some((960, _, _)) = held {
                    let cst = block_at_dim(dim, x, y, z);
                    if (4278..=4285).contains(&cst) {
                        let age = (cst - 4278) as u8;
                        let bump = 2 + ((x.wrapping_mul(7) ^ z.wrapping_mul(13) ^ y) & 1) as u8;
                        let nage = (age + bump).min(7);
                        set_block_dim(dim, x, y, z, 4278 + nage as u32);
                        crops().lock().unwrap().insert((dim, x, y, z), nage);
                        note_block_update_dim(dim, x, y, z, 4278 + nage as u32);
                        if *me.gm.lock().unwrap() != 1 {
                            let mut inv = me.inv.lock().unwrap();
                            if let Some((it, c, _cc)) = inv.get(&(36 + held_slot)).cloned() {
                                if c <= 1 {
                                    inv.remove(&(36 + held_slot));
                                } else {
                                    inv.insert(36 + held_slot, (it, c - 1, _cc));
                                }
                            }
                            let (it, c, _cc) =
                                inv.get(&(36 + held_slot))
                                    .cloned()
                                    .unwrap_or((0, 0, Vec::new()));
                            drop(inv);
                            let mut p = vec![0u8];
                            write_varint(&mut p, 0);
                            p.extend_from_slice(&(36 + held_slot).to_be_bytes());
                            p.extend_from_slice(&slot_wire(&(it, c, _cc)));
                            let _ = send_packet(&mut me.sock.lock().unwrap(), 0x15, p);
                        }
                        let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                        continue;
                    }
                }
                // hoe: convert dirt-family clicked block to farmland
                if let Some((item, _, _)) = held {
                    let is_hoe = (822..=847).contains(&item) && (item - 822) % 5 == 0;
                    let tillable = matches!(block_at_dim(dim, x, y, z), 9 | 10 | 12513);
                    if is_hoe && tillable && face == 1 {
                        let fm = name_to_state("farmland").unwrap_or(0);
                        if !place((x, y, z), fm) {
                            let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                            continue;
                        }
                        let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                        continue;
                    }
                    // wheat seeds on farmland -> wheat age0 + consume seed
                    if item == 853
                        && block_at_dim(dim, x, y, z) == name_to_state("farmland").unwrap_or(u32::MAX)
                    {
                        if !place((tx, ty, tz), 4278) {
                            let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                            continue;
                        }
                        crops().lock().unwrap().insert((dim, tx, ty, tz), 0);
                        if *me.gm.lock().unwrap() != 1 {
                            let mut inv = me.inv.lock().unwrap();
                            if let Some((it, c, _cc)) = inv.get(&(36 + held_slot)).cloned() {
                                if c <= 1 {
                                    inv.remove(&(36 + held_slot));
                                } else {
                                    inv.insert(36 + held_slot, (it, c - 1, _cc));
                                }
                            }
                            let (it, c, _cc) =
                                inv.get(&(36 + held_slot))
                                    .cloned()
                                    .unwrap_or((0, 0, Vec::new()));
                            drop(inv);
                            let mut p = vec![0u8];
                            write_varint(&mut p, 0);
                            p.extend_from_slice(&(36 + held_slot).to_be_bytes());
                            p.extend_from_slice(&slot_wire(&(it, c, _cc)));
                            let _ = send_packet(&mut me.sock.lock().unwrap(), 0x15, p);
                        }
                        let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                        continue;
                    }
                }
                if let Some((item, _cnt, _)) = held {
                    if let Some(sb) = sign_base_for_item(item) {
                        let yaw = {
                            let p = me.pos.lock().unwrap();
                            p.3
                        } as f64;
                        let rot = (((180.0 + yaw) * 16.0 / 360.0 + 0.5) as i64 & 15) as u32;
                        let st = sb + rot * 2 + 1;
                        if !place((tx, ty, tz), st) {
                            let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                            continue;
                        }
                        ac_consume_placed_item(&me, held_slot);
                        let mut op = Vec::new();
                        write_pos(&mut op, tx, ty, tz);
                        op.push(1);
                        let _ = send_packet(&mut me.sock.lock().unwrap(), 0x34, op);
                        let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                        continue;
                    }
                    if try_live_block_placement(
                        &me,
                        (dim, tx, ty, tz),
                        face.clamp(0, 5) as usize,
                        held_slot,
                        item,
                        clicked,
                    ) {
                        let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                        continue;
                    }
                    if let Some(st) = item_to_state(item) {
                        // A chest is placed with the state vanilla computes for it:
                        // facing from the player (opposite of the look direction),
                        // `type` from the neighbour it may pair with, and
                        // `waterlogged` from the fluid it replaces.
                        let st = match chest_kind(st) {
                            Some(name) => {
                                let yaw = me.pos.lock().unwrap().3;
                                let sneaking = *me.sneaking.lock().unwrap();
                                chest_placement_state_dim(
                                    dim,
                                    name,
                                    (tx, ty, tz),
                                    yaw,
                                    sneaking,
                                    face.clamp(0, 5) as u8,
                                )
                            }
                            None => st,
                        };
                        if place((tx, ty, tz), st) {
                            ac_consume_placed_item(&me, held_slot);
                        }
                    }
                }
                let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
            }
            0x36 => {
                // arm_animation: relay swing to others
                broadcast_seen(me.eid, |_c| {
                    (0x03, {
                        let mut p = Vec::new();
                        write_varint(&mut p, me.eid as i64);
                        p.push(0);
                        p
                    })
                });
            }
            0x39 => {
                // use_item: ack + eat + bow draw
                let mut sl = &b[..];
                let _h = read_varint(&mut sl);
                let seq = read_varint(&mut sl).unwrap_or(0);
                air_use_item(&me, held_slot, seq, _h, false);
            }
            0x04 | 0x05 => {
                // chat_command(+signed): command string (no leading /)
                let mut sl = &b[..];
                if let Some(mlen) = read_varint(&mut sl) {
                    let m =
                        String::from_utf8_lossy(&sl[..(mlen as usize).min(sl.len())]).to_string();
                    if !connection_limits::chat_text_valid(&m) {
                        eprintln!("[spine] {} disconnected: illegal chat command", me.name);
                        play_disconnect(&mut me.sock.lock().unwrap(), "Illegal characters in chat");
                        break 'conn;
                    }
                    if !chat_throttle.allow(Instant::now()) {
                        eprintln!("[spine] {} disconnected: chat spam", me.name);
                        play_disconnect(&mut me.sock.lock().unwrap(), "Kicked for spamming!");
                        break 'conn;
                    }

                    run_command(Some(&me), &format!("/{}", m), &mut Some(&me));
                }
            }
            0x06 => {
                // chat_message: the protocol-767 signed envelope runs the live
                // admission + relay in `live_chat_message` (P7a / #132).
                if let Err(text) = live_chat_message(&me, &b, &mut chat_throttle) {
                    eprintln!("[spine] {} disconnected: {}", me.name, text);
                    play_disconnect(&mut me.sock.lock().unwrap_or_else(|e| e.into_inner()), text);
                    break 'conn;
                }
            }
            0x12 => custom_payload_c2s(&me, &b),
            0x16 => {
                // use_entity: attack mobs
                let mut sl = &b[..];
                let target = read_varint(&mut sl).unwrap_or(-1) as i32;
                let mouse = read_varint(&mut sl).unwrap_or(-1);
                // vanilla entity_interaction_range = 3.0; gate all attack/interact
                // uses so distant entities can't be hit/sheared/bred cross-map.
                {
                    let (px, py, pz, ..) = *me.pos.lock().unwrap();
                    let vt: Option<Arc<Client>> = {
                        clients()
                            .lock()
                            .unwrap()
                            .iter()
                            .find(|c| c.eid == target)
                            .cloned()
                    };
                    let tp = vt
                        .map(|v| {
                            let p = *v.pos.lock().unwrap();
                            (p.0, p.1, p.2, false)
                        })
                        .or_else(|| {
                            let ms = mobs().lock().unwrap();
                            ms.iter().find(|m| m.eid == target).map(|m| {
                                let p = *m.pos.lock().unwrap();
                                (p.0, p.1, p.2, false)
                            })
                        })
                        .or_else(|| {
                            let gen = generated_content().lock().unwrap();
                            gen.carts
                                .iter()
                                .find(|c| c.eid == target)
                                .and_then(|c| c.pos().ok())
                                .map(|p| (p[0], p[1], p[2], true))
                        });
                    let ok = tp
                        .map(|(tx, ty, tz, is_cart)| {
                            let center_y = ty + if is_cart { 0.35 } else { 0.9 };
                            let (dx, dy, dz) = (tx - px, center_y - (py + 1.62), tz - pz);
                            dx * dx + dy * dy + dz * dz <= 3.2 * 3.2
                        })
                        .unwrap_or(false);
                    if !ok {
                        continue;
                    }
                }
                if mouse == 0 || mouse == 2 {
                    let cart_uuid = generated_content()
                        .lock()
                        .unwrap()
                        .carts
                        .iter()
                        .find(|c| c.eid == target)
                        .and_then(|c| c.uuid().ok());
                    if let Some(uuid) = cart_uuid {
                        if *me.gm.lock().unwrap() != 3 {
                            unpack_cart_loot_if_needed(uuid);
                            let key = ContainerKey::Cart(uuid);
                            let (audience, before) =
                                if let Some((prev, prev_anchor)) = client_open_window_and_anchor(&me)
                                {
                                    let aud = client_snapshot();
                                    let cdim = *me.dim.lock().unwrap_or_else(|e| e.into_inner());
                                    let halves =
                                        client_window_halves_dim(prev, prev_anchor, cdim);
                                    let snap = chest_opener_snapshot_dim(&aud, &halves);
                                    (aud, snap)
                                } else {
                                    (Vec::new(), Vec::new())
                                };
                            close_live_menu(&me, false, true);
                            let inv = me.inv.lock().unwrap().clone();
                            open_chest_for_client(
                                &me,
                                (key, None),
                                None,
                                &inv,
                                "Minecart with Chest",
                            );
                            if !before.is_empty() {
                                sync_chest_openers_dim(&audience, &before);
                            }
                        }
                        continue;
                    }
                    if pet_collar_or_parrot_interact(&me, target, 36 + held_slot) {
                        continue;
                    }
                    if live_mob_interact(&me, target, 36 + held_slot) {
                        continue;
                    }
                    // Boats, minecarts and the rideable mobs: `interact` on a
                    // free seat starts the ride and updates every tracker with
                    // the new SetPassengers list.
                    if try_mount_vehicle(&me, target) {
                        continue;
                    }
                    // interact: villagers open trade UI; wheat breeds animals
                    let villager = mobs()
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|m| m.eid == target && Some(m.etype) == name_to_etype("villager"))
                        .cloned();
                    let is_villager = villager.is_some();
                    let held_it = me
                        .inv
                        .lock()
                        .unwrap()
                        .get(&(36 + held_slot))
                        .cloned()
                        .unwrap_or((0, 0, Vec::new()))
                        .0;
                    if held_it == 983 && !is_villager {
                        // shears on sheep -> wool
                        let is_sheep = {
                            let ms = mobs().lock().unwrap();
                            ms.iter().any(|m| m.eid == target && m.etype == 87)
                        };
                        if is_sheep && !sheared().lock().unwrap().contains(&target) {
                            sheared().lock().unwrap().insert(target);
                            let (mx, my, mz) = {
                                let ms = mobs().lock().unwrap();
                                ms.iter()
                                    .find(|m| m.eid == target)
                                    .map(|m| *m.pos.lock().unwrap())
                                    .map(|p| (p.0, p.1, p.2))
                                    .unwrap_or((0.0, 0.0, 0.0))
                            };
                            let n = 1 + ((target as u64).wrapping_mul(2654435761) % 3) as u8;
                            for _ in 0..n {
                                let deid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
                                let d = Drop {
                                    eid: deid,
                                    x: mx,
                                    y: my + 0.5,
                                    z: mz,
                                    item: 202,
                                    cnt: 1,
                                    comps: Vec::new(),
                                    age: 0,
                                    target: None,
                                    thrower: None,
                                };
                                drops().lock().unwrap().push(d);
                            }
                            continue;
                        }
                    }
                    if held_it == 908 && !is_villager {
                        // bucket on cow/sheep -> milk
                        let milkable = {
                            let ms = mobs().lock().unwrap();
                            ms.iter()
                                .any(|m| m.eid == target && [22i64, 77].contains(&m.etype))
                        };
                        if milkable {
                            let k = 36 + held_slot;
                            {
                                let mut inv = me.inv.lock().unwrap();
                                let c = inv.get(&k).map(|v| v.1).unwrap_or(0);
                                if c <= 1 {
                                    inv.insert(k, (914, 1, Vec::new()));
                                } else {
                                    inv.insert(k, (908, c - 1, Vec::new()));
                                    let fs =
                                        (9..=44i16).find(|sl| !inv.contains_key(sl)).unwrap_or(9);
                                    inv.insert(fs, (914, 1, Vec::new()));
                                }
                            }
                            let (it2, c2, _c2) =
                                me.inv.lock().unwrap().get(&k).cloned().unwrap_or((
                                    0,
                                    0,
                                    Vec::new(),
                                ));
                            let mut p = vec![0u8];
                            write_varint(&mut p, 0);
                            p.extend_from_slice(&k.to_be_bytes());
                            p.extend_from_slice(&slot_wire(&(it2, c2, _c2)));
                            let _ = send_packet(&mut me.sock.lock().unwrap(), 0x15, p);
                            continue;
                        }
                    }
                    if matches!(held_it, 853 | 854 | 1097) && !is_villager {
                        // Keep the legacy wheat interaction and accept the GP-1 temptation items.
                        let mut loved = false;
                        {
                            let ms = mobs().lock().unwrap();
                            if let Some(m) = ms
                                .iter()
                                .find(|m| m.eid == target && match m.etype {
                                    19 => matches!(held_it, 853 | 854),
                                    77 => matches!(held_it, 854 | 1097),
                                    22 | 87 => held_it == 854,
                                    _ => false,
                                })
                            {
                                *m.love.lock().unwrap() = 80;
                                loved = true;
                            }
                        }
                        if loved {
                            if *me.gm.lock().unwrap() != 1 {
                                let mut inv = me.inv.lock().unwrap();
                                let k = 36 + held_slot;
                                if let Some((it, c, _kc)) = inv.get(&k).cloned() {
                                    if c > 1 {
                                        inv.insert(k, (it, c - 1, _kc));
                                    } else {
                                        inv.remove(&k);
                                    }
                                }
                                let (it, c, _cc) =
                                    inv.get(&k).cloned().unwrap_or((0, 0, Vec::new()));
                                drop(inv);
                                let mut p = vec![0u8];
                                write_varint(&mut p, 0);
                                p.extend_from_slice(&k.to_be_bytes());
                                p.extend_from_slice(&slot_wire(&(it, c, _cc)));
                                let _ = send_packet(&mut me.sock.lock().unwrap(), 0x15, p);
                            }
                            continue;
                        }
                    }
                    if let Some(villager) = villager {
                        open_live_merchant(&me, &villager);
                        continue;
                    }
                }
                if mouse == 1 {
                    // attack
                    // PvP: target may be another player
                    if target != me.eid {
                        let gm = *me.gm.lock().unwrap();
                        if gm != 3 && break_generated_cart(target, gm != 1) {
                            continue;
                        }
                        let mut found: Option<(i32, String, f32, f32)> = None;
                        {
                            // LOCK ORDER: snapshot, never hold clients() while
                            // broadcasting or touching per-client locks
                            // (self-deadlock: broadcast re-locks clients()).
                            let vic: Option<Arc<Client>> = {
                                clients()
                                    .lock()
                                    .unwrap()
                                    .iter()
                                    .find(|c| c.eid == target)
                                    .cloned()
                            };
                            if let Some(v) = vic.as_ref() {
                                if *v.gm.lock().unwrap() == 1 {
                                } else {
                                    // vanilla: attack_damage attribute (fist=1) + held item bonus + sharpness
                                    let held = {
                                        let i2 = me.inv.lock().unwrap();
                                        i2.get(&(36 + held_slot)).cloned().unwrap_or((
                                            0,
                                            0,
                                            Vec::new(),
                                        ))
                                    };
                                    let band = if (818..=847).contains(&held.0) {
                                        ((held.0 - 818) / 5) as i64
                                    } else {
                                        -1
                                    };
                                    let kind = if (818..=847).contains(&held.0) {
                                        (held.0 - 818) % 5
                                    } else {
                                        -1
                                    };
                                    // vanilla tool attack damages by kind (sword,shovel,pick,axe,hoe)
                                    // bands: wood,stone,gold,iron,diamond,netherite
                                    let wep = match kind {
                                        0 => [4.0, 5.0, 4.0, 6.0, 7.0, 8.0],  // swords
                                        1 => [2.5, 3.5, 2.5, 4.5, 5.5, 6.5],  // shovels
                                        2 => [2.0, 3.0, 2.0, 4.0, 5.0, 6.0],  // pickaxes
                                        3 => [7.0, 9.0, 7.0, 9.0, 9.0, 10.0], // axes
                                        4 => [1.0, 1.0, 1.0, 1.0, 1.0, 1.0],  // hoes
                                        _ => [1.0; 6],
                                    }[band.clamp(0, 5) as usize];
                                    let wep = if held.0 == 1188 {
                                        9.0
                                    } else if held.0 == 1093 {
                                        5.0
                                    } else {
                                        wep
                                    };
                                    let sharp = enchant_level(&held.2, 32) as f32 * 0.5;
                                    // attr default 1 = fist; an attribute-set value adds on top
                                    let chg = melee_charge(&me, held.0);
                                    let crt = {
                                        let (_px, py, _pz, _, _, g) = *me.pos.lock().unwrap();
                                        let ff = *me.fall_from.lock().unwrap();
                                        !g && py < ff - 0.1
                                    };
                                    let (strb, wkb) = {
                                        let ef = me.effs.lock().unwrap();
                                        (
                                            ef.iter()
                                                .find(|e| e.id == 4)
                                                .map(|e| e.amp + 1)
                                                .unwrap_or(0)
                                                as f32
                                                * 3.0,
                                            ef.iter()
                                                .find(|e| e.id == 17)
                                                .map(|e| e.amp + 1)
                                                .unwrap_or(0)
                                                as f32
                                                * -4.0,
                                        )
                                    };
                                    let dmg = ((attr_of(
                                        &format!("p:{}", me.name),
                                        "generic.attack_damage",
                                    ) as f32
                                        - 1.0)
                                        .max(0.0)
                                        + wep
                                        + sharp
                                        + strb
                                        + wkb)
                                        .max(0.0)
                                        * (0.2 + 0.8 * chg * chg)
                                        * if crt { 1.5 } else { 1.0 };
                                    let Some((accepted, starts_hurt_window)) =
                                        damage_after_invulnerability(v, dmg)
                                    else {
                                        continue;
                                    };
                                    let dmg = accepted;
                                    *me.walk_acc.lock().unwrap() += 0.1;
                                    // vanilla: an axe hit disables a raised shield for 5s
                                    if kind == 3 && *v.blocking.lock().unwrap() {
                                        *v.blocking.lock().unwrap() = false;
                                        *v.shield_cd.lock().unwrap() = Instant::now();
                                    }
                                    let vb = kind != 3
                                        && *v.blocking.lock().unwrap()
                                        && v.shield_cd.lock().unwrap().elapsed()
                                            > Duration::from_secs(5);
                                    if vb {
                                        damage_shield(v, dmg);
                                    }
                                    let mut h = v.hp.lock().unwrap();
                                    *h = (*h
                                        - if vb {
                                            0.0
                                        } else {
                                            hurt_absorb(v, dmg_after(v, dmg, 0))
                                        })
                                    .max(0.0);
                                    if *h <= 0.0 {
                                        drop(h);
                                        totem_check(v);
                                        h = v.hp.lock().unwrap();
                                    }
                                    let hh = *h;
                                    drop(h);
                                    if !vb {
                                        damage_armor(v, dmg);
                                    }
                                    if damage_held(&me) {
                                        let (pxx, pyy, pzz, ..) = {
                                            let p = me.pos.lock().unwrap();
                                            *p
                                        };
                                        let snd =
                                            sound_pkt_named("entity.item.break", pxx, pyy, pzz);
                                        broadcast_near(
                                            *me.dim.lock().unwrap(),
                                            pxx,
                                            pyy,
                                            pzz,
                                            -1,
                                            |_c| (0x68, snd.clone()),
                                        );
                                    }
                                    held_slot_resync(&me);
                                    if starts_hurt_window && !vb && dmg > 0.0 {
                                        let attacker_dim = *me.dim.lock().unwrap();
                                        let victim_dim = *v.dim.lock().unwrap();
                                        if attacker_dim == victim_dim {
                                            broadcast_player_attack_damage(
                                                v.eid, me.eid, victim_dim,
                                            );
                                        }
                                    }
                                    let (pp, vp) = (
                                        {
                                            let p = me.pos.lock().unwrap();
                                            *p
                                        },
                                        {
                                            let p = v.pos.lock().unwrap();
                                            *p
                                        },
                                    );
                                    // vanilla LivingEntity.hurt → indicateDamage:
                                    // hurtDir = atan2(dz, dx) * 57.2957763671875 - victimYaw
                                    // where (dx, dz) is attacker minus victim position.
                                    let hurt_yaw = {
                                        let dx = pp.0 - vp.0;
                                        let dz = pp.2 - vp.2;
                                        (dz.atan2(dx) * 57.2957763671875
                                            - f64::from(vp.3)) as f32
                                    };
                                    let hurt_pkt = hurt_anim_yaw(v.eid, hurt_yaw);
                                    if let Ok(mut w) = v.sock.lock() {
                                        let _ = send_packet(&mut w, 0x24, hurt_pkt.clone());
                                        let _ = send_packet(&mut w, 0x5d, health_pkt_c(hh, v));
                                    }
                                    // vanilla LivingEntity.knockback(0.4, dx, dz): the
                                    // strength is scaled by (1 - knockback_resistance),
                                    // the direction is the normalized attacker-minus-
                                    // victim offset, and SetEntityMotion clamps each
                                    // axis to +-3.9 and truncates velocity * 8000.
                                    let kb = vanilla_knockback(&vp, &pp);
                                    let vel = velocity_pkt(v.eid, kb.0, kb.1, kb.2);
                                    if let Ok(mut w) = v.sock.lock() {
                                        let _ = send_packet(&mut w, 0x5a, vel.clone());
                                    }
                                    found = Some((v.eid, v.name.clone(), hh, hurt_yaw));
                                }
                            }
                        }
                        if let Some((veid, vname, hh, hurt_yaw)) = found {
                            let hurt_pkt = hurt_anim_yaw(veid, hurt_yaw);
                            broadcast_seen_ex(veid, veid, |_c| (0x24, hurt_pkt.clone()));
                            if hh <= 0.0 {
                                let victim = {
                                    clients().lock().unwrap().iter().find(|c| c.eid == veid).cloned()
                                };
                                if let Some(victim) = victim {
                                    drop_player_items_on_death(&victim);
                                }
                                if GR_SHOW_DEATH_MESSAGES.load(Ordering::Relaxed) != 0 {
                                    let line = system_chat(&format!(
                                        "{} was slain by {}",
                                        vname, me.name
                                    ));
                                    broadcast(-1, |_c| (0x6c, line.clone()));
                                }
                            }
                            continue;
                        }
                    }
                    let mut dead: Vec<(i32, i64, f64, f64, f64)> = Vec::new();
                    let mut swept: Vec<i32> = Vec::new();
                    {
                        let ms = mobs().lock().unwrap();
                        if let Some(m) = ms.iter().find(|m| m.eid == target) {
                            if m.dim != *me.dim.lock().unwrap() {
                                continue;
                            }
                            // vanilla hurtTime: 0.5s of invulnerability after a hit
                            let fresh =
                                m.hurt_at.lock().unwrap().elapsed() >= Duration::from_millis(500);
                            if fresh {
                                if !live_mob_hit_allowed(m, me.eid, BossHit::Other) {
                                    continue;
                                }
                                *m.hurt_at.lock().unwrap() = Instant::now();
                            }
                            if fresh {
                                *me.walk_acc.lock().unwrap() += 0.1;
                                let held = {
                                    let i2 = me.inv.lock().unwrap();
                                    i2.get(&(36 + held_slot))
                                        .cloned()
                                        .unwrap_or((0, 0, Vec::new()))
                                };
                                // vanilla melee: weapon total by tool kind; sharpness adds 0.5*lvl
                                let band = if (818..=847).contains(&held.0) {
                                    ((held.0 - 818) / 5) as i64
                                } else {
                                    -1
                                };
                                let kind = if (818..=847).contains(&held.0) {
                                    (held.0 - 818) % 5
                                } else {
                                    -1
                                };
                                let wep = match kind {
                                    0 => [4.0, 5.0, 4.0, 6.0, 7.0, 8.0],
                                    1 => [2.5, 3.5, 2.5, 4.5, 5.5, 6.5],
                                    2 => [2.0, 3.0, 2.0, 4.0, 5.0, 6.0],
                                    3 => [7.0, 9.0, 7.0, 9.0, 9.0, 10.0],
                                    4 => [1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
                                    _ => [1.0; 6],
                                }[band.clamp(0, 5) as usize];
                                let wep = if held.0 == 1188 {
                                    9.0
                                } else if held.0 == 1093 {
                                    5.0
                                } else {
                                    wep
                                };
                                let sharp = enchant_level(&held.2, 32) as f32 * 0.5;
                                // smite (+2.5/lvl vs undead) / bane (+2.5/lvl vs arthropods)
                                let undead = enchantment_targets_undead(m.etype);
                                let arthro = enchantment_targets_arthropod(m.etype);
                                let spec = if undead {
                                    enchant_level(&held.2, 34)
                                } else if arthro {
                                    enchant_level(&held.2, 1)
                                } else {
                                    0
                                } as f32
                                    * 2.5;
                                // vanilla attack cooldown: charge<1 scales damage 0.2..1.0
                                let charge = melee_charge(&me, held.0);
                                let scale = 0.2 + 0.8 * charge * charge;
                                // Vanilla criticals require a charged attack and exclude
                                // sprinting, water, climbable blocks, and blindness.
                                let (px, py, pz, _, _, grounded) = *me.pos.lock().unwrap();
                                let dim = *me.dim.lock().unwrap();
                                let (bx, by, bz) = (
                                    px.floor() as i32,
                                    py.floor() as i32,
                                    pz.floor() as i32,
                                );
                                let reg = crate::block_registry::breg();
                                let in_water = reg.is_some_and(|r| {
                                    r.has_water(block_at_dim(dim, bx, by, bz))
                                        || r.has_water(block_at_dim(dim, bx, by + 1, bz))
                                });
                                let climbing = reg.is_some_and(|r| {
                                    [
                                        block_at_dim(dim, bx, by, bz),
                                        block_at_dim(dim, bx, by + 1, bz),
                                    ]
                                        .into_iter()
                                        .any(|state| {
                                            matches!(
                                                r.name_of(state),
                                                "minecraft:ladder"
                                                    | "minecraft:vine"
                                                    | "minecraft:scaffolding"
                                                    | "minecraft:weeping_vines"
                                                    | "minecraft:weeping_vines_plant"
                                                    | "minecraft:twisting_vines"
                                                    | "minecraft:twisting_vines_plant"
                                                    | "minecraft:cave_vines"
                                                    | "minecraft:cave_vines_plant"
                                            )
                                        })
                                });
                                let blind = me
                                    .effs
                                    .lock()
                                    .unwrap()
                                    .iter()
                                    .any(|effect| effect.id == 14);
                                let ff = *me.fall_from.lock().unwrap();
                                let crit = melee_critical_allowed(
                                    charge,
                                    grounded,
                                    py < ff - 0.1,
                                    *me.fall_fly.lock().unwrap(),
                                    *me.sprinting.lock().unwrap(),
                                    in_water,
                                    climbing,
                                    blind,
                                );
                                let (strb, wkb) = {
                                    let ef = me.effs.lock().unwrap();
                                    (
                                        ef.iter()
                                            .find(|e| e.id == 4)
                                            .map(|e| e.amp + 1)
                                            .unwrap_or(0)
                                            as f32
                                            * 3.0,
                                        ef.iter()
                                            .find(|e| e.id == 17)
                                            .map(|e| e.amp + 1)
                                            .unwrap_or(0)
                                            as f32
                                            * -4.0,
                                    )
                                };
                                let mut h = m.hp.lock().unwrap();
                                *h -= ((wep + sharp + spec + strb + wkb).max(0.0))
                                    * scale
                                    * if crit { 1.5 } else { 1.0 };
                                drop(h);
                                // Vanilla swords sweep even without Sweeping Edge.
                                if kind == 0 {
                                    let (mx, my, mz, ..) = *m.pos.lock().unwrap();
                                    let (width, height) = entity_dimensions_1211(m.etype);
                                    let target_box = PlayerAabb {
                                        min: [mx - f64::from(width) / 2.0, my, mz - f64::from(width) / 2.0],
                                        max: [mx + f64::from(width) / 2.0, my + f64::from(height), mz + f64::from(width) / 2.0],
                                    };
                                    for other in ms.iter().filter(|other| other.eid != m.eid && other.dim == dim) {
                                        let (ox, oy, oz, ..) = *other.pos.lock().unwrap();
                                        let (ow, oh) = entity_dimensions_1211(other.etype);
                                        let candidate = PlayerAabb {
                                            min: [ox - f64::from(ow) / 2.0, oy, oz - f64::from(ow) / 2.0],
                                            max: [ox + f64::from(ow) / 2.0, oy + f64::from(oh), oz + f64::from(ow) / 2.0],
                                        };
                                        let sweep = sweeping_attack(
                                            charge,
                                            grounded,
                                            *me.sprinting.lock().unwrap(),
                                            crit,
                                            [px, py, pz],
                                            target_box,
                                            candidate,
                                            (wep + sharp + strb + wkb).max(0.0),
                                            enchant_level(&held.2, 36) as i32,
                                        );
                                        if !sweep.eligible || !live_mob_hit_allowed(other, me.eid, BossHit::Other) {
                                            continue;
                                        }
                                        let mut hurt_at = other.hurt_at.lock().unwrap();
                                        if hurt_at.elapsed() < Duration::from_millis(500) {
                                            continue;
                                        }
                                        *hurt_at = Instant::now();
                                        drop(hurt_at);
                                        let mut health = other.hp.lock().unwrap();
                                        if *health <= 0.0 {
                                            continue;
                                        }
                                        *health -= sweep.damage;
                                        if *health <= 0.0 {
                                            dead.push((other.eid, other.etype, ox, oy, oz));
                                        }
                                        drop(health);
                                        swept.push(other.eid);
                                    }
                                }
                                // Fire Aspect adds four seconds of independent burn time per level.
                                let fa = enchant_level(&held.2, 10);
                                if fa > 0 && !mob_fire_immune(m.etype) {
                                    *m.fire_ticks.lock().unwrap() = fire_aspect_duration_ticks(fa);
                                }
                                if damage_held(&me) {
                                    let (pxx, pyy, pzz, ..) = {
                                        let p = me.pos.lock().unwrap();
                                        *p
                                    };
                                    let snd = sound_pkt_named("entity.item.break", pxx, pyy, pzz);
                                    broadcast_near(
                                        *me.dim.lock().unwrap(),
                                        pxx,
                                        pyy,
                                        pzz,
                                        -1,
                                        |_c| (0x68, snd.clone()),
                                    );
                                }
                                held_slot_resync(&me);
                                let h = m.hp.lock().unwrap();
                                if *h <= 0.0 {
                                    let (mx, my, mz, ..) = {
                                        let p = m.pos.lock().unwrap();
                                        *p
                                    };
                                    dead.push((m.eid, m.etype, mx, my, mz));
                                }
                                let ha = hurt_anim(m.eid);
                                let _ =
                                    send_packet(&mut me.sock.lock().unwrap(), 0x24, ha.clone());
                                broadcast_seen_ex(me.eid, me.eid, |_c| (0x24, ha.clone()));
                                // knockback away from attacker
                                let (pp, mp) = (
                                    {
                                        let p = me.pos.lock().unwrap();
                                        *p
                                    },
                                    {
                                        let p = m.pos.lock().unwrap();
                                        *p
                                    },
                                );
                                let (mut kx, mut kz) = (mp.0 - pp.0, mp.2 - pp.2);
                                let kl = (kx * kx + kz * kz).sqrt().max(0.01);
                                kx /= kl;
                                kz /= kl;
                                let vp = velocity_pkt(
                                    m.eid,
                                    (kx * 3200.0) as i16,
                                    2800,
                                    (kz * 3200.0) as i16,
                                );
                                broadcast_seen(m.eid, |_c| (0x5a, vp.clone()));
                                // vanilla: sprint attacks add knockback and reset the sprint
                                let sprint_hit = *me.sprinting.lock().unwrap();
                                if sprint_hit {
                                    *me.sprinting.lock().unwrap() = false;
                                }
                                let kbs = 0.6
                                    + 0.6 * enchant_level(&held.2, 17) as f64
                                    + if sprint_hit { 0.5 } else { 0.0 };
                                let (nx, nz) = (mp.0 + kx * kbs, mp.2 + kz * kbs);
                                {
                                    let mut p = m.pos.lock().unwrap();
                                    p.0 = nx;
                                    p.2 = nz;
                                }
                                let tp = entity_teleport(m.eid, nx, mp.1, nz, mp.3, mp.4, true);
                                broadcast_seen(m.eid, |_c| (0x70, tp.clone()));
                            }
                        }
                    }
                    for target_eid in swept {
                        broadcast_seen(target_eid, |_c| (0x24, hurt_anim(target_eid)));
                    }
                    for (deid, etype, mx, my, mz) in dead {
                        adv_event(&me.name, "player_killed_entity", Some(etype));
                        stat_increment(&me, StatCategory::Killed, etype as i32, 1);
                        let mn = match etype {
                            124 => "Zombie",
                            22 => "Cow",
                            23 => "Creeper",
                            91 => "Skeleton",
                            19 => "Chicken",
                            77 => "Pig",
                            87 => "Sheep",
                            100 => "Spider",
                            _ => "mob",
                        };
                        if GR_SHOW_DEATH_MESSAGES.load(Ordering::Relaxed) != 0 {
                            let line =
                                system_chat(&format!("{} was slain by {}", mn, me.name));
                            broadcast(-1, |_c| (0x6c, line.clone()));
                        }
                        let Some((mob, state)) = remove_live_mob(deid) else { continue };
                        let burn_kill = *mob.fire_ticks.lock().unwrap() > 0;
                        live_mob_death(&mob, state, [mx, my, mz]);
                        broadcast_seen_destroy(deid);
                        let was_sheared = sheared().lock().unwrap().remove(&deid);
                        // A dead vehicle drops its riders, a dead rider leaves
                        // its seat, and the mob's equipment record dies with it.
                        live_mount_forget(deid);
                        mob_equipment().lock().unwrap().remove(&deid);
                        // looting: +0..level extra count per drop
                        let held2 = {
                            let i2 = me.inv.lock().unwrap();
                            i2.get(&(36 + held_slot))
                                .cloned()
                                .unwrap_or((0, 0, Vec::new()))
                        };
                        let loot = enchant_level(&held2.2, 18).clamp(0, 3) as u64;
                        for (item, cnt, comps) in mob_loot(etype, burn_kill)
                            .into_iter()
                            .filter(|st| !(was_sheared && st.0 == 202))
                            .filter(|_| GR_MOB_LOOT.load(Ordering::Relaxed) == 1)
                        {
                            let extra = if loot > 0 {
                                (world_rng::next_u64() % (loot + 1)) as u8
                            } else {
                                0
                            };
                            let d = Drop {
                                eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                                x: mx,
                                y: my,
                                z: mz,
                                item,
                                cnt: cnt + extra,
                                comps,
                                age: 0,
                                target: None,
                                thrower: None,
                            };
                            drops().lock().unwrap().push(d);
                        }
                        let xeid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
                        xps().lock().unwrap().push(Xp {
                            eid: xeid,
                            x: mx,
                            y: my,
                            z: mz,
                            val: mob_xp(etype),
                            age: 0,
                        });
                        // spawn streams via interest_tick
                    }
                }
            }
            0x28 => {
                // ServerboundRecipeBookChangeSettingsPacket
                let mut sl = &b[..];
                if let Some(s) = decode_recipe_book_change_settings(&mut sl) {
                    recipe_books()
                        .lock()
                        .unwrap()
                        .entry(me.uuid)
                        .or_insert_with(PlayerRecipeBook::new)
                        .apply_book_settings(s.book_type, s.book_open, s.filter_active);
                }
            }
            0x29 => {
                // ServerboundRecipeBookSeenRecipePacket
                let mut sl = &b[..];
                if let Some(id) = decode_recipe_book_seen_recipe(&mut sl) {
                    recipe_books()
                        .lock()
                        .unwrap()
                        .entry(me.uuid)
                        .or_insert_with(PlayerRecipeBook::new)
                        .seen_recipe(&id);
                }
            }
            0x22 => {
                // ServerboundPlaceRecipePacket
                let mut sl = &b[..];
                if let Some(place) = decode_place_recipe(&mut sl) {
                    place_live_recipe(&me, &place);
                }
            }
            0x09 => {
                // client_command: 0 = perform respawn, 1 = request stats
                // (`ServerboundClientCommandPacket` actions).
                let mut sl = &b[..];
                let action = read_varint(&mut sl);
                if action == Some(1) && sl.is_empty() {
                    let body = {
                        let mut stats = player_stats().lock().unwrap();
                        let stats = stats.entry(me.uuid).or_insert_with(PlayerStats::new);
                        // P8b-2: the protocol-767 `award_stats` payload comes
                        // from the P8b-2 codec over the live counter book —
                        // same `(category, stat, value)` triples the save codec
                        // encodes, in `BTreeMap` order.
                        let entries: Vec<(u8, i32, i32)> = stats
                            .counts
                            .iter()
                            .map(|(&(category, stat_id), &value)| {
                                (category.id() as u8, stat_id, value)
                            })
                            .collect();
                        encode_award_stats_0x04(&entries)
                    };
                    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x04, body);
                } else if action == Some(0) && sl.is_empty() {
                    drop_player_items_on_death(&me);
                    let restored_health = max_hp_of(&me.name);
                    {
                        let mut health = me.hp.lock().unwrap();
                        if *health > 0.0 {
                            continue;
                        }
                        *health = restored_health;
                    }
                    // `ServerPlayer.stopRiding` on death: the respawn starts
                    // unmounted wherever the death happened.
                    player_dismount(&me, false);
                    death_drop_marks().lock().unwrap().remove(&me.eid);
                    close_live_menu(&me, true, true);
                    // P2-3-S4: respawn in the dimension the spawn point belongs to. A
                    // death in the Nether returns to the Overworld bed/world spawn when that
                    // is where the spawn point was set, with a full window rebuild.
                    let (sdim, (rx, ry, rz, yaw)) = {
                        let sdim = me.spawn_dim.lock().unwrap();
                        let point = *me.spawn_pt.lock().unwrap();
                        match point {
                            Some(point) => (*sdim, point),
                            None => (DimId::Overworld, *WORLD_SPAWN.lock().unwrap()),
                        }
                    };
                    switch_client_dimension(&me, sdim, (rx, ry, rz, yaw, 0.0), view);
                    reset_player_food(&me);
                    let _ = send_player_xp(&me, |_| Ok(()));
                    {
                        let iv = me.inv.lock().unwrap();
                        *me.armor_pts.lock().unwrap() = armor_pts_of(&iv);
                    }
                    let _ = send_packet(
                        &mut me.sock.lock().unwrap(),
                        0x5d,
                        health_pkt_c(max_hp_of(&me.name), &me),
                    );
                    send_attrs(&me);
                    // vanilla resyncs window 0 after respawn — without it
                    // the client keeps showing the pre-death inventory
                    let mut q = vec![0u8]; // window 0 = player inventory
                    write_varint(&mut q, 0);
                    write_varint(&mut q, 46);
                    {
                        let iv = me.inv.lock().unwrap();
                        for i in 0..46i16 {
                            let (it, c, _cc) = iv.get(&i).cloned().unwrap_or((0, 0, Vec::new()));
                            q.extend_from_slice(&slot_wire(&(it, c, _cc)));
                        }
                    }
                    q.extend_from_slice(&slot_wire(&(0, 0, Vec::new()))); // carried
                    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x13, q);
                    let inventory = me.inv.lock().unwrap().clone();
                    save_live_player_snapshot(&me, client_dim_pos(&me), &inventory);
                }
            }
            0x2b => {
                // resource_pack_receive: status varint
                let mut sl = &b[..];
                let status = read_varint(&mut sl);
                eprintln!("[spine] resource_pack status {status:?}");
                if resource_pack_status_disconnects(status) {
                    play_disconnect(
                        &mut me.sock.lock().unwrap(),
                        "This server requires the configured resource pack.",
                    );
                    break 'conn;
                }
            }
            0x35 => {
                // update_sign: pos i64, front bool, 4 strings
                if b.len() >= 10 {
                    let (sx, sy, sz) = read_pos(&b[..8]);
                    let front = if b[8] != 0 { 1u8 } else { 0u8 };
                    let mut sl = &b[9..];
                    let mut lines: [String; 4] = Default::default();
                    for l in lines.iter_mut() {
                        let n = read_varint(&mut sl).unwrap_or(0).clamp(0, 256) as usize;
                        let m = n.min(sl.len());
                        *l = String::from_utf8_lossy(&sl[..m]).into_owned();
                        sl = &sl[m..];
                    }
                    let dim = *me.dim.lock().unwrap();
                    let pos = *me.pos.lock().unwrap();
                    // ServerPlayer's sign edit path checks mayInteract and the
                    // eight-block squared distance before changing the block entity.
                    if !matches!(*me.gm.lock().unwrap(), 0 | 1)
                        || !sign_edit_in_range((pos.0, pos.1, pos.2), sx, sy, sz)
                        || !is_sign_block_state(block_at_dim(dim, sx, sy, sz))
                    {
                        continue;
                    }
                    signs()
                        .lock()
                        .unwrap()
                        .insert((dim, sx, sy, sz, front), lines.clone());
                    broadcast_chunk_dim(dim, sx.div_euclid(16), sz.div_euclid(16), |_c| {
                        (0x07, sign_entity_pkt(sx, sy, sz, front, &lines))
                    });
                }
            }
            0x32 => {
                // set_creative_slot: slot i16 + slot data (creative only)
                if b.len() >= 3 && *me.gm.lock().unwrap() == 1 {
                    let Some((slot, st)) = slot_read_wire(&b) else {
                        continue;
                    };
                    // Player inventory menu slots are crafting output/input, armor,
                    // inventory, hotbar and offhand (0..=45).
                    if !creative_inventory_slot(slot) {
                        continue;
                    }
                    if st.1 > 0 {
                        me.inv.lock().unwrap().insert(slot, st.clone());
                    } else {
                        me.inv.lock().unwrap().remove(&slot);
                    }
                    {
                        let iv = me.inv.lock().unwrap();
                        *me.armor_pts.lock().unwrap() = armor_pts_of(&iv);
                    }
                    // echo container set_slot (0x15): window 0, stateId 0
                    let mut p = vec![0u8];
                    write_varint(&mut p, 0);
                    p.extend_from_slice(&slot.to_be_bytes());
                    p.extend_from_slice(&slot_wire(&st));
                    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x15, p);
                }
            }
            0x0d => {
                let mut payload = b.as_slice();
                if let (Some(window), Some(button)) =
                    (read_protocol_i32(&mut payload), read_protocol_i32(&mut payload))
                {
                    if payload.is_empty() {
                        with_save_guard(|| live_enchant_button(&me, window, button));
                    }
                }
            }
            0x2a => {
                let mut payload = b.as_slice();
                if let Some(length) = read_protocol_i32(&mut payload) {
                    if (0..=150).contains(&length) && payload.len() == length as usize {
                        if let Ok(name) = std::str::from_utf8(payload) {
                            with_save_guard(|| live_anvil_rename(&me, name));
                        }
                    }
                }
            }
            0x2d => {
                if let Some(index) = decode_select_trade(&b) {
                    with_save_guard(|| live_select_trade(&me, index));
                }
            }
            0x2e => {
                // set_beacon_effect: Optional<varint> primary, Optional<varint>
                // secondary — mob-effect registry ids, not BeaconEffect ordinals.
                let mut sl = &b[..];
                let opt = |sl: &mut &[u8]| -> Option<Option<i64>> {
                    match *sl.first()? {
                        0 => {
                            *sl = &sl[1..];
                            Some(None)
                        }
                        1 => {
                            *sl = &sl[1..];
                            read_varint(sl).map(Some)
                        }
                        _ => None,
                    }
                };
                let Some(primary) = opt(&mut sl) else {
                    continue;
                };
                let Some(secondary) = opt(&mut sl) else {
                    continue;
                };
                let primary = primary.and_then(beacon_effect_from_wire);
                let secondary = secondary.and_then(beacon_effect_from_wire);
                let dim = *me.dim.lock().unwrap();
                if let Some((bdim, bx, by, bz)) = live_beacon_anchor(&me, dim) {
                    let applied = with_save_guard(|| {
                        let Some(reg) = crate::block_registry::breg() else {
                            return false;
                        };
                        let state = block_at_dim(bdim, bx, by, bz);
                        let mut store = live_block_entities().lock().unwrap();
                        let Some(LiveBlockEntity::Beacon(beacon)) =
                            store.at(reg, (bdim, bx, by, bz), state)
                        else {
                            return false;
                        };
                        let held_slot = 36 + *me.held.lock().unwrap();
                        let stack = me
                            .inv
                            .lock()
                            .unwrap()
                            .get(&held_slot)
                            .cloned()
                            .unwrap_or_default();
                        let mut payment = BlockEntityItem::from_stack(&stack);
                        if !beacon.confirm(primary, secondary, &mut payment) {
                            return false;
                        }
                        map_take(&mut me.inv.lock().unwrap(), held_slot, 1);
                        true
                    });
                    if applied {
                        // The beacon's changed state is the confirmation: the
                        // updated block-entity data goes out to every holder of
                        // its chunk, matching sendBlockUpdated.
                        if let Some((_, tag)) =
                            live_block_entity_snapshot((bdim, bx, by, bz))
                        {
                            if let Ok(nbt) = crate::nbt::write_network_nbt(&tag) {
                                let mut pkt = Vec::new();
                                write_pos(&mut pkt, bx, by, bz);
                                write_varint(&mut pkt, 14); // block entity type: beacon
                                pkt.extend_from_slice(&nbt);
                                broadcast_chunk_dim(bdim, bx >> 4, bz >> 4, |_c| {
                                    (0x07, pkt.clone())
                                });
                                held_slot_resync(&me);
                            }
                        }
                    }
                }
            }
            0x0f => {
                if let Some(id) = live_menu_id(&me) {
                    if b.as_slice() == [id] {
                        close_live_menu_id(&me, Some(id), false, true);
                    }
                    continue;
                }
                if b.first().is_some_and(|&id| id >= 4) {
                    continue;
                }
                // close_window
                // `AbstractContainerMenu.removed` -> `Container.stopOpen`: the opener
                // counters drop and each half that moved broadcasts its BlockEvent.
                let chest_window = client_open_window_and_anchor(&me);
                if let Some((open, anchor)) = chest_window {
                    let audience = client_snapshot();
                    let cdim = *me.dim.lock().unwrap_or_else(|e| e.into_inner());
                    let halves = client_window_halves_dim(open, anchor, cdim);
                    let before = chest_opener_snapshot_dim(&audience, &halves);
                    *me.open_chest.lock().unwrap() = None;
                    *me.chest_sync.lock().unwrap() = None;
                    sync_chest_openers_dim(&audience, &before);
                }
                let had_container = chest_window.is_some()
                    || me.open_furnace.lock().unwrap().is_some()
                    || *me.open_table.lock().unwrap();
                *me.open_chest.lock().unwrap() = None;
                *me.chest_sync.lock().unwrap() = None;
                *me.open_furnace.lock().unwrap() = None;
                *me.open_table.lock().unwrap() = false;
                beacon_menu_anchors().lock().unwrap().remove(&me.eid);
                if had_container {
                    // vanilla containerMenu.removed: the carried stack is
                    // thrown into the world, crafting grid goes back to inv
                    let cur =
                        std::mem::take(&mut *me.cursor.lock().unwrap());
                    if cur.1 > 0 {
                        let (px, py, pz, ..) = *me.pos.lock().unwrap();
                        drops().lock().unwrap().push(Drop {
                            eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                            x: px,
                            y: py + 1.3,
                            z: pz,
                            item: cur.0,
                            cnt: cur.1,
                            comps: cur.2,
                            age: -(server_ticks(40) as i32), // pickup delay: 2s already elapsed,
                            target: None,
                            thrower: None,
                        });
                    }
                    let mut inv = me.inv.lock().unwrap();
                    for g in [&me.craft, &me.craft9] {
                        for (_, st) in std::mem::take(&mut *g.lock().unwrap()) {
                            if st.1 > 0 {
                                let slot = (9..=44).find(|sl| !inv.contains_key(sl)).unwrap_or(9);
                                inv.insert(slot, st);
                            }
                        }
                    }
                    drop(inv);
                    *me.drag.lock().unwrap() = (-1, Vec::new());
                }
            }
            0x0e
                // window_click: window u8, stateId varint, slot i16, button i8, mode varint, ...
                if b.len() >= 6 => {
                    let mut sl = &b[..];
                    let wid = sl[0];
                    sl = &sl[1..];
                    let Some(click_state_id) = read_protocol_i32(&mut sl) else {
                        continue;
                    };
                    if sl.len() >= 4 {
                        let slot = i16::from_be_bytes(sl[..2].try_into().unwrap());
                        let btn = sl[2];
                        sl = &sl[3..];
                        let Some(mode) = read_protocol_i32(&mut sl).map(i64::from) else {
                            continue;
                        };
                        let click_tail = sl;
                        if let Some(active) = live_menu_id(&me) {
                            if wid == active {
                                // Vanilla never rejects on a stale stateId: it
                                // pushes the authoritative contents first, then
                                // processes the click anyway.
                                let _ = live_menu_connection(&me).is_some_and(|conn| {
                                    let mut session = conn.active.lock().unwrap();
                                    match session.as_mut() {
                                        Some(s)
                                            if s.id == wid && s.state_id != click_state_id =>
                                        {
                                            sync_live_menu(&me, s, false);
                                            true
                                        }
                                        _ => false,
                                    }
                                });
                                with_save_guard(|| live_menu_click(&me, wid, slot, btn as i8, mode));
                            }
                            continue;
                        }
                        let active = if me.open_chest.lock().unwrap().is_some() {
                            1
                        } else if me.open_furnace.lock().unwrap().is_some() {
                            2
                        } else if *me.open_table.lock().unwrap() {
                            3
                        } else {
                            0
                        };
                        if wid != active {
                            continue;
                        }
                        // Vanilla never rejects on a stale stateId: push the
                        // authoritative SetContainerContent snapshot with a
                        // fresh id, then process the click anyway.
                        if container_state_stale(&me, wid, click_state_id) {
                            resend_container_content(&me, wid);
                        }
                        let creative = *me.gm.lock().unwrap() == 1;
                        if wid == 0 && (5..=8).contains(&slot) && matches!(mode, 0 | 1 | 4) {
                            let stack = me.inv.lock().unwrap().get(&slot).cloned();
                            if armor_removal_blocked(slot, stack.as_ref(), creative) {
                                resync_slot(&me, 0, slot, &stack.unwrap());
                                let cursor = me.cursor.lock().unwrap().clone();
                                resync_slot(&me, 0, -1, &cursor);
                                continue;
                            }
                        }
                        if mode == 0 {
                            let chest_pos = *me.open_chest.lock().unwrap();
                            if let Some((cp, cp2)) = chest_pos {
                                if !window_live_for(&me, (cp, cp2)) {
                                    invalidate_client_chest_window(
                                        &client_snapshot(),
                                        &me,
                                        (cp, cp2),
                                    );
                                    continue;
                                }
                                // chest window: single 0-26 chest,27-62 inv | double 0-26 A,27-53 B,54-89 inv
                                let rows = if cp2.is_some() { 54i16 } else { 27i16 };
                                let mut all = chests().lock().unwrap();
                                let mut inv = me.inv.lock().unwrap();
                                let mut cur = me.cursor.lock().unwrap();
                                if slot == -999 {
                                    if cur.1 > 0 {
                                        // vanilla: left-click outside = throw whole stack, right-click = one
                                        let n = if btn == 1 { 1 } else { cur.1 };
                                        let it = cur.clone();
                                        if n >= cur.1 {
                                            *cur = (0, 0, Vec::new());
                                        } else {
                                            cur.1 -= n;
                                        }
                                        let (px, py, pz, ..) = {
                                            let p = me.pos.lock().unwrap();
                                            *p
                                        };
                                        let d = Drop {
                                            eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                                            x: px,
                                            y: py + 1.3,
                                            z: pz,
                                            item: it.0,
                                            cnt: n,
                                            comps: it.2.clone(),
                                            age: 0,
                                            target: None,
                                            thrower: Some(me.uuid),
                                        };
                                        drops().lock().unwrap().push(d);
                                    }
                                } else if slot >= 0 && slot < rows + 36 {
                                    let (gslot, gmap): (i16, &mut HashMap<i16, Stack>) =
                                        if slot < 27 {
                                            (slot, all.entry(cp).or_default())
                                        } else if slot < rows {
                                            (slot - 27, all.entry(cp2.unwrap()).or_default())
                                        } else {
                                            (slot - rows + 9, &mut inv)
                                        };
                                    click_slot(gmap, &mut cur, gslot, btn as i8);
                                    let st = gmap.get(&gslot).cloned().unwrap_or((0, 0, Vec::new()));
                                    drop(all);
                                    drop(inv);
                                    drop(cur);
                                    send_chest_slot(&me, slot, &st);
                                }
                                continue;
                            }
                            let furnace_pos = *me.open_furnace.lock().unwrap();
                            if let Some(fp) = furnace_pos {
                                let mut fs = furnaces().lock().unwrap();
                                let f = fs.entry(fp).or_insert(Furnace {
                                    input: (0, 0, Vec::new()),
                                    fuel: (0, 0, Vec::new()),
                                    output: (0, 0, Vec::new()),
                                    burn_left: 0,
                                    progress: 0,
                                    dirty: false,
                                    xp: 0.0,
                                    recipe_cache: CachedLookup::default(),
                                    fuel_cache: CachedLookup::default(),
                                });
                                let mut inv = me.inv.lock().unwrap();
                                let mut cur = me.cursor.lock().unwrap();
                                if slot == -999 {
                                    if cur.1 > 0 {
                                        // vanilla: left-click outside = throw whole stack, right-click = one
                                        let n = if btn == 1 { 1 } else { cur.1 };
                                        let it = cur.clone();
                                        if n >= cur.1 {
                                            *cur = (0, 0, Vec::new());
                                        } else {
                                            cur.1 -= n;
                                        }
                                        let (px, py, pz, ..) = {
                                            let p = me.pos.lock().unwrap();
                                            *p
                                        };
                                        let d = Drop {
                                            eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                                            x: px,
                                            y: py + 1.3,
                                            z: pz,
                                            item: it.0,
                                            cnt: n,
                                            comps: it.2.clone(),
                                            age: 0,
                                            target: None,
                                            thrower: Some(me.uuid),
                                        };
                                        drops().lock().unwrap().push(d);
                                    }
                                } else if (0..39).contains(&slot) {
                                    if slot == 2 {
                                        // output: take only, merge onto cursor
                                        if f.output.1 > 0
                                            && (cur.1 == 0
                                                || (cur.0 == f.output.0
                                                    && cur.2 == f.output.2
                                                    && cur.1 + f.output.1
                                                        <= item_stack_max(f.output.0, &f.output.2)
                                                            as u8))
                                        {
                                            let got = f.output.1;
                                            *cur = (f.output.0, cur.1 + got, f.output.2.clone());
                                            f.output = (0, 0, Vec::new());
                                            f.dirty = true;
                                            let (px, py, pz, ..) = {
                                                let p = me.pos.lock().unwrap();
                                                *p
                                            };
                                            furnace_xp_pop(f, px, py, pz);
                                        }
                                    } else if slot == 0 || slot == 1 {
                                        let m = if slot == 0 { &mut f.input } else { &mut f.fuel };
                                        let sv = if m.1 == 0 { None } else { Some(m.clone()) };
                                        let cv = cur.clone();
                                        if btn == 0 {
                                            if cv.1 == 0 {
                                                if let Some(s) = sv {
                                                    *cur = s;
                                                    *m = (0, 0, Vec::new());
                                                }
                                            } else {
                                                match sv {
                                                    None => {
                                                        *m = cv;
                                                        *cur = (0, 0, Vec::new());
                                                    }
                                                    Some((si, sc, _cc)) => {
                                                        if si == cv.0
                                                            && _cc == cv.2
                                                            && sc as i64 + cv.1 as i64 <= item_stack_max(si, &_cc)
                                                        {
                                                            *m = (si, sc + cv.1, _cc);
                                                            *cur = (0, 0, Vec::new());
                                                        } else {
                                                            *m = cv;
                                                            *cur = (si, sc, _cc);
                                                        }
                                                    }
                                                }
                                            }
                                        } else if btn == 1 {
                                            if cv.1 == 0 {
                                                if let Some((si, sc, _cc)) = sv {
                                                    let take = sc - sc / 2;
                                                    *cur = (si, take, _cc.clone());
                                                    if sc - take == 0 {
                                                        *m = (0, 0, Vec::new());
                                                    } else {
                                                        *m = (si, sc - take, _cc);
                                                    }
                                                }
                                            } else {
                                                match sv {
                                                    None => {
                                                        *m = (cv.0, 1, cv.2.clone());
                                                        *cur = (cv.0, cv.1 - 1, cv.2);
                                                    }
                                                    Some((si, sc, _cc)) => {
                                                        if si == cv.0 && _cc == cv.2 && sc < item_stack_max(si, &_cc) as u8 {
                                                            *m = (si, sc + 1, _cc);
                                                            *cur = (cv.0, cv.1 - 1, cv.2);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        f.dirty = true;
                                    } else {
                                        let islot = slot + 6;
                                        click_slot(&mut inv, &mut cur, islot, btn as i8);
                                        *me.armor_pts.lock().unwrap() = armor_pts_of(&inv);
                                    }
                                }
                                // Snapshot the changed slots before acquiring the socket mutex.
                                let slots = [f.input.clone(), f.fuel.clone(), f.output.clone()];
                                let player_slot = (3..39).contains(&slot).then(|| {
                                    (slot, inv.get(&(slot + 6)).cloned().unwrap_or((0, 0, Vec::new())))
                                });
                                drop(cur);
                                drop(inv);
                                drop(fs);
                                if (0..39).contains(&slot) {
                                    let mut sock = me.sock.lock().unwrap();
                                    for (wsl, st) in slots.iter().enumerate() {
                                        let mut p = vec![2u8];
                                        write_varint(&mut p, 0);
                                        p.extend_from_slice(&(wsl as i16).to_be_bytes());
                                        p.extend_from_slice(&slot_wire(st));
                                        let _ = send_packet(&mut sock, 0x15, p);
                                    }
                                    if let Some((slot, st)) = player_slot {
                                        let mut p = vec![2u8];
                                        write_varint(&mut p, 0);
                                        p.extend_from_slice(&slot.to_be_bytes());
                                        p.extend_from_slice(&slot_wire(&st));
                                        let _ = send_packet(&mut sock, 0x15, p);
                                    }
                                }
                                continue;
                            }
                            if *me.open_table.lock().unwrap() {
                                let mut grid = me.craft9.lock().unwrap();
                                let mut inv = me.inv.lock().unwrap();
                                let mut cur = me.cursor.lock().unwrap();
                                if slot == -999 {
                                    if cur.1 > 0 {
                                        let n = if btn == 1 { 1 } else { cur.1 };
                                        let it = cur.clone();
                                        if n >= cur.1 {
                                            *cur = (0, 0, Vec::new());
                                        } else {
                                            cur.1 -= n;
                                        }
                                        let (px, py, pz, ..) = {
                                            let p = me.pos.lock().unwrap();
                                            *p
                                        };
                                        let d = Drop {
                                            eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                                            x: px,
                                            y: py + 1.3,
                                            z: pz,
                                            item: it.0,
                                            cnt: n,
                                            comps: it.2.clone(),
                                            age: 0,
                                            target: None,
                                            thrower: Some(me.uuid),
                                        };
                                        drops().lock().unwrap().push(d);
                                        drop(cur);
                                        drop(grid);
                                        drop(inv);
                                        // spawn streams via interest_tick
                                        // drop meta streams via interest_tick
                                        continue;
                                    }
                                } else if slot == 0 {
                                    if let Some((ri, rc, _rc)) = craft3_result(&grid) {
                                        if cur.1 == 0
                                            || (cur.0 == ri
                                                && cur.2 == _rc
                                                && cur.1 + rc <= item_stack_max(ri, &_rc) as u8)
                                        {
                                            for i in 1..=9i16 {
                                                if let Some((it, c, _cc)) = grid.get(&i).cloned() {
                                                    if c <= 1 {
                                                        grid.remove(&i);
                                                    } else {
                                                        grid.insert(i, (it, c - 1, _cc));
                                                    }
                                                }
                                            }
                                            *cur = (ri, cur.1 + rc, _rc.clone());
                                        }
                                    }
                                } else if (1..=9).contains(&slot) {
                                    click_slot(&mut grid, &mut cur, slot, btn as i8);
                                } else if (10..=45).contains(&slot) {
                                    click_slot(&mut inv, &mut cur, slot - 1, btn as i8);
                                    *me.armor_pts.lock().unwrap() = armor_pts_of(&inv);
                                }
                                // echo changed slot + result
                                let (it, c, _ce) = if slot == 0 {
                                    (0, 0, Vec::new())
                                } else if (1..=9).contains(&slot) {
                                    grid.get(&slot).cloned().unwrap_or((0, 0, Vec::new()))
                                } else if slot >= 10 {
                                    inv.get(&(slot - 1)).cloned().unwrap_or((0, 0, Vec::new()))
                                } else {
                                    (0, 0, Vec::new())
                                };
                                let out = craft3_result(&grid).unwrap_or((0, 0, Vec::new()));
                                drop(grid);
                                drop(inv);
                                drop(cur);
                                if slot >= 0 {
                                    let mut p = vec![3u8];
                                    write_varint(&mut p, 0);
                                    p.extend_from_slice(&slot.to_be_bytes());
                                    p.extend_from_slice(&slot_wire(&(it, c, _ce)));
                                    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x15, p);
                                }
                                let mut p0 = vec![3u8];
                                write_varint(&mut p0, 0);
                                p0.extend_from_slice(&0i16.to_be_bytes());
                                p0.extend_from_slice(&slot_wire(&(out.0, out.1, Vec::new())));
                                let _ = send_packet(&mut me.sock.lock().unwrap(), 0x15, p0);
                                continue;
                            }
                            // Window 0 (player inventory + 2x2 craft grid). Vanilla's
                            // post-click broadcast emits only the slots whose content
                            // changed, in menu order (result, grid, armor, inventory,
                            // offhand), then the carried stack when it differs from the
                            // stack the client acknowledged in this click.
                            let pre_grid = me.craft.lock().unwrap().clone();
                            let pre_inv = me.inv.lock().unwrap().clone();
                            let pre_result =
                                craft_result(&pre_grid).unwrap_or((0, 0, Vec::new()));
                            if slot == -999 {
                                let cur = {
                                    let c = me.cursor.lock().unwrap();
                                    c.clone()
                                };
                                if cur.1 > 0 {
                                    let n = if btn == 1 { 1 } else { cur.1 };
                                    if n >= cur.1 {
                                        *me.cursor.lock().unwrap() = (0, 0, Vec::new());
                                    } else {
                                        me.cursor.lock().unwrap().1 -= n;
                                    }
                                    let (px, py, pz, ..) = {
                                        let p = me.pos.lock().unwrap();
                                        *p
                                    };
                                    let d = Drop {
                                        eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                                        x: px,
                                        y: py + 1.3,
                                        z: pz,
                                        item: cur.0,
                                        cnt: n,
                                        comps: cur.2.clone(),
                                        age: 0,
                                        target: None,
                                        thrower: Some(me.uuid),
                                    };
                                    drops().lock().unwrap().push(d);
                                }
                            } else if slot == 0 {
                                // craft output: take result, consume one of each ingredient
                                let mut cur = me.cursor.lock().unwrap();
                                let mut grid = me.craft.lock().unwrap();
                                if let Some((ri, rc, _rc)) = craft_result(&grid) {
                                    if cur.1 == 0
                                        || (cur.0 == ri
                                            && cur.2 == _rc
                                            && cur.1 + rc <= item_stack_max(ri, &_rc) as u8)
                                    {
                                        for i in 1..=4i16 {
                                            if let Some((it, c, _cc)) = grid.get(&i).cloned() {
                                                if c <= 1 {
                                                    grid.remove(&i);
                                                } else {
                                                    grid.insert(i, (it, c - 1, _cc));
                                                }
                                            }
                                        }
                                        *cur = (ri, cur.1 + rc, _rc.clone());
                                    }
                                }
                            } else if (1..=4).contains(&slot) {
                                let mut grid = me.craft.lock().unwrap();
                                let mut cur = me.cursor.lock().unwrap();
                                click_slot(&mut grid, &mut cur, slot, btn as i8);
                            } else if (5..=8).contains(&slot) || slot == 45 {
                                // armor + offhand: vanilla restricts armor
                                // slots to their matching piece type
                                let armor_slot_ok = |it: i64, sl: i16| -> bool {
                                    if it == 0 {
                                        return true;
                                    }
                                    let n = |x: &str| name_to_item(x).unwrap_or(-1);
                                    match sl {
                                        5 => {
                                            it == n("leather_helmet")
                                                || it == n("golden_helmet")
                                                || it == n("chainmail_helmet")
                                                || it == n("iron_helmet")
                                                || it == n("diamond_helmet")
                                                || it == n("netherite_helmet")
                                                || it == n("turtle_helmet")
                                        }
                                        6 => {
                                            it == n("leather_chestplate")
                                                || it == n("golden_chestplate")
                                                || it == n("chainmail_chestplate")
                                                || it == n("iron_chestplate")
                                                || it == n("diamond_chestplate")
                                                || it == n("netherite_chestplate")
                                                || it == n("elytra")
                                        }
                                        7 => {
                                            it == n("leather_leggings")
                                                || it == n("golden_leggings")
                                                || it == n("chainmail_leggings")
                                                || it == n("iron_leggings")
                                                || it == n("diamond_leggings")
                                                || it == n("netherite_leggings")
                                        }
                                        8 => {
                                            it == n("leather_boots")
                                                || it == n("golden_boots")
                                                || it == n("chainmail_boots")
                                                || it == n("iron_boots")
                                                || it == n("diamond_boots")
                                                || it == n("netherite_boots")
                                        }
                                        _ => true,
                                    }
                                };
                                let mut inv = me.inv.lock().unwrap();
                                let mut cur = me.cursor.lock().unwrap();
                                if cur.1 > 0 && armor_slot_ok(cur.0, slot) || cur.1 == 0 {
                                    click_slot(&mut inv, &mut cur, slot, btn as i8);
                                    *me.armor_pts.lock().unwrap() = armor_pts_of(&inv);
                                }
                            } else if (9..=44).contains(&slot) {
                                let mut inv = me.inv.lock().unwrap();
                                let mut cur = me.cursor.lock().unwrap();
                                click_slot(&mut inv, &mut cur, slot, btn as i8);
                                *me.armor_pts.lock().unwrap() = armor_pts_of(&inv);
                            }
                            let post_grid = me.craft.lock().unwrap().clone();
                            let post_inv = me.inv.lock().unwrap().clone();
                            let post_cur = me.cursor.lock().unwrap().clone();
                            let post_result =
                                craft_result(&post_grid).unwrap_or((0, 0, Vec::new()));
                            let carried = click_carried_stack(click_tail);
                            let mut sock = me.sock.lock().unwrap();
                            if post_result != pre_result {
                                let mut p = vec![0u8];
                                write_varint(&mut p, 0);
                                p.extend_from_slice(&0i16.to_be_bytes());
                                p.extend_from_slice(&slot_wire(&post_result));
                                let _ = send_packet(&mut sock, 0x15, p);
                            }
                            for i in 1..=4i16 {
                                let now = post_grid
                                    .get(&i)
                                    .cloned()
                                    .unwrap_or((0, 0, Vec::new()));
                                let was = pre_grid
                                    .get(&i)
                                    .cloned()
                                    .unwrap_or((0, 0, Vec::new()));
                                if now != was {
                                    let mut p = vec![0u8];
                                    write_varint(&mut p, 0);
                                    p.extend_from_slice(&i.to_be_bytes());
                                    p.extend_from_slice(&slot_wire(&now));
                                    let _ = send_packet(&mut sock, 0x15, p);
                                }
                            }
                            for i in 5..=45i16 {
                                let now = post_inv
                                    .get(&i)
                                    .cloned()
                                    .unwrap_or((0, 0, Vec::new()));
                                let was = pre_inv
                                    .get(&i)
                                    .cloned()
                                    .unwrap_or((0, 0, Vec::new()));
                                if now != was {
                                    let mut p = vec![0u8];
                                    write_varint(&mut p, 0);
                                    p.extend_from_slice(&i.to_be_bytes());
                                    p.extend_from_slice(&slot_wire(&now));
                                    let _ = send_packet(&mut sock, 0x15, p);
                                }
                            }
                            let cursor_differs = match carried {
                                Some(st) => st != post_cur,
                                None => post_cur.1 > 0,
                            };
                            if cursor_differs {
                                let mut p = vec![0xFFu8];
                                write_varint(&mut p, 0);
                                p.extend_from_slice(&(-1i16).to_be_bytes());
                                p.extend_from_slice(&slot_wire(&post_cur));
                                let _ = send_packet(&mut sock, 0x15, p);
                            }
                        } else if mode == 2 {
                            // number-key swap: hovered slot <-> hotbar slot btn (0-8)
                            let hb = 36 + btn as i16;
                            if (0..=8).contains(&btn) {
                                let chest_pos = *me.open_chest.lock().unwrap();
                                if let Some((cp, cp2)) = chest_pos {
                                    if !window_live_for(&me, (cp, cp2)) {
                                        invalidate_client_chest_window(
                                            &client_snapshot(),
                                            &me,
                                            (cp, cp2),
                                        );
                                        continue;
                                    }
                                }
                                let furnace_pos = *me.open_furnace.lock().unwrap();
                                let mut inv = me.inv.lock().unwrap();
                                if let Some((cp, cp2)) = chest_pos {
                                    let rows = if cp2.is_some() { 54i16 } else { 27i16 };
                                    let mut all = chests().lock().unwrap();
                                    if slot >= 0 && slot < rows + 36 {
                                        let hst = inv.remove(&hb).unwrap_or((0, 0, Vec::new()));
                                        if slot < rows {
                                            let (pos, gslot) = if slot < 27 {
                                                (cp, slot)
                                            } else {
                                                (cp2.unwrap(), slot - 27)
                                            };
                                            let m = all.entry(pos).or_default();
                                            let sst =
                                                m.remove(&gslot).unwrap_or((0, 0, Vec::new()));
                                            if hst.1 > 0 {
                                                m.insert(gslot, hst.clone());
                                            }
                                            if sst.1 > 0 {
                                                inv.insert(hb, sst);
                                            }
                                        } else {
                                            let g = slot - rows + 9;
                                            let sst = inv.remove(&g).unwrap_or((0, 0, Vec::new()));
                                            if sst.1 > 0 {
                                                inv.insert(hb, sst);
                                            }
                                            if hst.1 > 0 {
                                                inv.insert(g, hst.clone());
                                            }
                                            resync_slot(&me, 1, slot, &hst);
                                            resync_slot(
                                                &me,
                                                1,
                                                rows + (hb - 9),
                                                &inv.get(&hb).cloned().unwrap_or((
                                                    0,
                                                    0,
                                                    Vec::new(),
                                                )),
                                            );
                                            drop(all);
                                            drop(inv);
                                            continue;
                                        }
                                        let cur_st = {
                                            let (pos, gslot) = if slot < 27 {
                                                (cp, slot)
                                            } else {
                                                (cp2.unwrap(), slot - 27)
                                            };
                                            all.get(&pos)
                                                .and_then(|m| m.get(&gslot))
                                                .cloned()
                                                .unwrap_or((0, 0, Vec::new()))
                                        };
                                        resync_slot(&me, 1, slot, &cur_st);
                                        resync_slot(
                                            &me,
                                            1,
                                            rows + (hb - 9),
                                            &inv.get(&hb).cloned().unwrap_or((0, 0, Vec::new())),
                                        );
                                    }
                                } else if let Some(fp) = furnace_pos {
                                    let mut fs = furnaces().lock().unwrap();
                                    if (0..39).contains(&slot) {
                                        let hst = inv.remove(&hb).unwrap_or((0, 0, Vec::new()));
                                        if slot >= 3 {
                                            let g = slot + 6;
                                            let sst = inv.remove(&g).unwrap_or((0, 0, Vec::new()));
                                            if sst.1 > 0 {
                                                inv.insert(hb, sst);
                                            }
                                            if hst.1 > 0 {
                                                inv.insert(g, hst.clone());
                                            }
                                            resync_slot(&me, 2, slot, &hst);
                                        } else {
                                            let f = fs.entry(fp).or_insert(Furnace {
                                                input: (0, 0, Vec::new()),
                                                fuel: (0, 0, Vec::new()),
                                                output: (0, 0, Vec::new()),
                                                burn_left: 0,
                                                progress: 0,
                                                dirty: false,
                                                xp: 0.0,
                                                recipe_cache: CachedLookup::default(),
                                                fuel_cache: CachedLookup::default(),
                                            });
                                            if slot == 2 {
                                                // output is take-only — push it to hotbar, don't swap in
                                                if f.output.1 > 0 {
                                                    let st = std::mem::take(&mut f.output);
                                                    f.dirty = true;
                                                    let (px, py, pz, ..) = {
                                                        let p = me.pos.lock().unwrap();
                                                        *p
                                                    };
                                                    furnace_xp_pop(f, px, py, pz);
                                                    let rem = move_to_slots(&mut inv, 9..=44, st);
                                                    if rem.1 > 0 {
                                                        fs.get_mut(&fp).unwrap().output = rem;
                                                    }
                                                }
                                            } else {
                                                let m = if slot == 0 {
                                                    &mut f.input
                                                } else {
                                                    &mut f.fuel
                                                };
                                                let sst = std::mem::take(m);
                                                if hst.1 > 0 {
                                                    *m = hst.clone();
                                                }
                                                f.dirty = true;
                                                if sst.1 > 0 {
                                                    inv.insert(hb, sst);
                                                }
                                            }
                                        }
                                        resync_slot(
                                            &me,
                                            2,
                                            hb - 6,
                                            &inv.get(&hb).cloned().unwrap_or((0, 0, Vec::new())),
                                        ); // inv hotbar window slot = 30 + (hb-36)+9?
                                    }
                                } else {
                                    // inventory window: swap slot with hotbar (armor slots refuse)
                                    let swappables = (9..=44).contains(&slot) || slot == 45;
                                    if swappables {
                                        let a = inv.remove(&slot).unwrap_or((0, 0, Vec::new()));
                                        let b = inv.remove(&hb).unwrap_or((0, 0, Vec::new()));
                                        if b.1 > 0 {
                                            inv.insert(slot, b.clone());
                                        }
                                        if a.1 > 0 {
                                            inv.insert(hb, a.clone());
                                        }
                                        resync_slot(&me, 0, slot, &b);
                                        resync_slot(&me, 0, hb, &a);
                                    }
                                }
                                *me.armor_pts.lock().unwrap() = armor_pts_of(&inv);
                            }
                        } else if mode == 6 {
                            // double-click: gather all matching stacks in the
                            // window onto the cursor (up to stack cap)
                            let chest_pos = *me.open_chest.lock().unwrap();
                            if let Some((cp, cp2)) = chest_pos {
                                if !window_live_for(&me, (cp, cp2)) {
                                    invalidate_client_chest_window(
                                        &client_snapshot(),
                                        &me,
                                        (cp, cp2),
                                    );
                                    continue;
                                }
                            }
                            let mut cur = me.cursor.lock().unwrap();
                            let mut inv = me.inv.lock().unwrap();
                            if cur.1 > 0 {
                                let cap = item_stack_max(cur.0, &cur.2) as i64;
                                if let Some((cp, cp2)) = chest_pos {
                                    let mut all = chests().lock().unwrap();
                                    for pos in [cp].into_iter().chain(cp2) {
                                        let m = all.entry(pos).or_default();
                                        let ks: Vec<i16> = m
                                            .iter()
                                            .filter(|(_, v)| v.0 == cur.0 && v.2 == cur.2)
                                            .map(|(&k, _)| k)
                                            .collect();
                                        for k in ks {
                                            if cur.1 as i64 >= cap {
                                                break;
                                            }
                                            let take =
                                                (cap - cur.1 as i64).min(m[&k].1 as i64) as u8;
                                            m.get_mut(&k).unwrap().1 -= take;
                                            cur.1 += take;
                                            if m[&k].1 == 0 {
                                                m.remove(&k);
                                            }
                                        }
                                    }
                                }
                                if (cur.1 as i64) < cap {
                                    let ks: Vec<i16> = inv
                                        .iter()
                                        .filter(|(&slot, stack)| {
                                            stack.0 == cur.0
                                                && stack.2 == cur.2
                                                && !armor_removal_blocked(slot, Some(stack), creative)
                                        })
                                        .map(|(&k, _)| k)
                                        .collect();
                                    for k in ks {
                                        if cur.1 as i64 >= cap {
                                            break;
                                        }
                                        let take = (cap - cur.1 as i64).min(inv[&k].1 as i64) as u8;
                                        inv.get_mut(&k).unwrap().1 -= take;
                                        cur.1 += take;
                                        if inv[&k].1 == 0 {
                                            inv.remove(&k);
                                        }
                                    }
                                }
                            }
                            let cp2 = cur.clone();
                            drop(inv);
                            drop(cur);
                            // cursor echo (window slot -1) + whole-inventory refresh is cheapest
                            let mut pc = vec![0u8];
                            write_varint(&mut pc, 0);
                            pc.extend_from_slice(&(-1i16).to_be_bytes());
                            pc.extend_from_slice(&slot_wire(&cp2));
                            if let Ok(mut w) = me.sock.lock() {
                                let _ = send_packet(&mut w, 0x15, pc);
                            }
                        } else if mode == 5 {
                            // drag-paint: btn 0/4 start (left/right), 1/5 add slot, 2/6 end
                            let bt = btn as i32;
                            let mut dr = me.drag.lock().unwrap();
                            match bt {
                                0 | 4 => {
                                    dr.0 = bt;
                                    dr.1.clear();
                                }
                                1 | 5 => {
                                    if dr.0 >= 0 && dr.0 + 1 == bt {
                                        dr.1.push(slot);
                                    }
                                }
                                2 | 6
                                    if dr.0 >= 0 && dr.0 + 2 == bt => {
                                        let slots_taken = std::mem::take(&mut dr.1);
                                        dr.0 = -1;
                                        drop(dr);
                                        let mut cur = me.cursor.lock().unwrap();
                                        if cur.1 > 0 && !slots_taken.is_empty() {
                                            let per = (cur.1 as usize / slots_taken.len()) as u8;
                                            // vanilla right-drag paints exactly 1 per slot
                                            let per = if dr_btn_right(bt) { 1 } else { per };
                                            if per > 0 {
                                                for wsl in slots_taken {
                                                    paint_slot(&me, wsl, per, &mut cur);
                                                }
                                            }
                                        }
                                    }
                                _ => {}
                            }
                        } else if mode == 4 || mode == 1 {
                            // vanilla click modes: 4=throw hovered slot (btn 0=one,
                            // 1=whole stack); 1=shift-move between regions
                            let chest_pos = *me.open_chest.lock().unwrap();
                            if let Some((cp, cp2)) = chest_pos {
                                if !window_live_for(&me, (cp, cp2)) {
                                    invalidate_client_chest_window(
                                        &client_snapshot(),
                                        &me,
                                        (cp, cp2),
                                    );
                                    continue;
                                }
                            }
                            let furnace_pos = *me.open_furnace.lock().unwrap();
                            let (px, py, pz, ..) = {
                                let p = me.pos.lock().unwrap();
                                *p
                            };
                            macro_rules! throw {
                                ($st:expr) => {{
                                    let st: Stack = $st;
                                    if st.1 > 0 {
                                        drops().lock().unwrap().push(Drop {
                                            eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                                            x: px,
                                            y: py + 1.3,
                                            z: pz,
                                            item: st.0,
                                            cnt: st.1,
                                            comps: st.2,
                                            age: 0,
                                            target: None,
                                            thrower: Some(me.uuid),
                                        });
                                    }
                                }};
                            }
                            if let Some((cp, cp2)) = chest_pos {
                                // window slots: 0..rows chest | rows..rows+36 inv(key=slot-rows+9)
                                let rows = if cp2.is_some() { 54i16 } else { 27i16 };
                                let mut all = chests().lock().unwrap();
                                let mut inv = me.inv.lock().unwrap();
                                if slot >= 0 && slot < rows + 36 {
                                    let in_cont = slot < rows;
                                    if mode == 4 {
                                        let n = if btn == 1 { 255 } else { 1 };
                                        let got = if in_cont {
                                            let (pos, gslot) = if slot < 27 {
                                                (cp, slot)
                                            } else {
                                                (cp2.unwrap(), slot - 27)
                                            };
                                            map_take(all.entry(pos).or_default(), gslot, n)
                                        } else {
                                            map_take(&mut inv, slot - rows + 9, n)
                                        };
                                        if let Some(st) = got {
                                            throw!(st);
                                        }
                                    } else {
                                        quick_move_chest(&mut all, &mut inv, cp, cp2, slot);
                                    }
                                    // resync touched slot + inventory/chest echo kept cheap
                                    let cur_st = if in_cont {
                                        let (pos, gslot) = if slot < 27 {
                                            (cp, slot)
                                        } else {
                                            (cp2.unwrap(), slot - 27)
                                        };
                                        all.get(&pos)
                                            .and_then(|m| m.get(&gslot))
                                            .cloned()
                                            .unwrap_or((0, 0, Vec::new()))
                                    } else {
                                        inv.get(&(slot - rows + 9)).cloned().unwrap_or((
                                            0,
                                            0,
                                            Vec::new(),
                                        ))
                                    };
                                    resync_slot(&me, 1, slot, &cur_st);
                                }
                                continue;
                            }
                            if let Some(fp) = furnace_pos {
                                // window: 0 input,1 fuel,2 output | 3..38 inv(key=slot+6)
                                let mut fs = furnaces().lock().unwrap();
                                let mut inv = me.inv.lock().unwrap();
                                if (0..39).contains(&slot) {
                                    if mode == 4 {
                                        if slot >= 3 {
                                            if let Some(st) = map_take(
                                                &mut inv,
                                                slot + 6,
                                                if btn == 1 { 255 } else { 1 },
                                            ) {
                                                throw!(st);
                                            }
                                        } else {
                                            let f = fs.entry(fp).or_insert(Furnace {
                                                input: (0, 0, Vec::new()),
                                                fuel: (0, 0, Vec::new()),
                                                output: (0, 0, Vec::new()),
                                                burn_left: 0,
                                                progress: 0,
                                                dirty: false,
                                                xp: 0.0,
                                                recipe_cache: CachedLookup::default(),
                                                fuel_cache: CachedLookup::default(),
                                            });
                                            let m = if slot == 0 {
                                                &mut f.input
                                            } else if slot == 1 {
                                                &mut f.fuel
                                            } else {
                                                &mut f.output
                                            };
                                            let n = if btn == 1 { m.1 } else { 1.min(m.1) };
                                            if n > 0 {
                                                let st = (m.0, n, m.2.clone());
                                                m.1 -= n;
                                                if m.1 == 0 {
                                                    *m = (0, 0, Vec::new());
                                                }
                                                f.dirty = true;
                                                if slot == 2 {
                                                    let (px, py, pz, ..) = {
                                                        let p = me.pos.lock().unwrap();
                                                        *p
                                                    };
                                                    furnace_xp_pop(f, px, py, pz);
                                                }
                                                throw!(st);
                                            }
                                        }
                                    } else if slot == 2 {
                                        // shift-take output into inventory
                                        let f = fs.entry(fp).or_insert(Furnace {
                                            input: (0, 0, Vec::new()),
                                            fuel: (0, 0, Vec::new()),
                                            output: (0, 0, Vec::new()),
                                            burn_left: 0,
                                            progress: 0,
                                            dirty: false,
                                            xp: 0.0,
                                            recipe_cache: CachedLookup::default(),
                                            fuel_cache: CachedLookup::default(),
                                        });
                                        if f.output.1 > 0 {
                                            let st = std::mem::take(&mut f.output);
                                            f.dirty = true;
                                            let (px, py, pz, ..) = {
                                                let p = me.pos.lock().unwrap();
                                                *p
                                            };
                                            furnace_xp_pop(f, px, py, pz);
                                            let rem = move_to_slots(&mut inv, 9..=44, st);
                                            if rem.1 > 0 {
                                                fs.get_mut(&fp).unwrap().output = rem;
                                            }
                                        }
                                    } else if slot <= 1 {
                                        // shift input/fuel -> inv
                                        let f = fs.entry(fp).or_insert(Furnace {
                                            input: (0, 0, Vec::new()),
                                            fuel: (0, 0, Vec::new()),
                                            output: (0, 0, Vec::new()),
                                            burn_left: 0,
                                            progress: 0,
                                            dirty: false,
                                            xp: 0.0,
                                            recipe_cache: CachedLookup::default(),
                                            fuel_cache: CachedLookup::default(),
                                        });
                                        let m = if slot == 0 { &mut f.input } else { &mut f.fuel };
                                        if m.1 > 0 {
                                            let st = std::mem::take(m);
                                            f.dirty = true;
                                            let rem = move_to_slots(&mut inv, 9..=44, st);
                                            if rem.1 > 0 {
                                                let f2 = fs.get_mut(&fp).unwrap();
                                                if slot == 0 {
                                                    f2.input = rem
                                                } else {
                                                    f2.fuel = rem
                                                }
                                            }
                                        }
                                    } else {
                                        // shift inv -> furnace input/fuel by type, else stays
                                        if let Some(st) = inv.get(&(slot + 6)).cloned() {
                                            let f = fs.entry(fp).or_insert(Furnace {
                                                input: (0, 0, Vec::new()),
                                                fuel: (0, 0, Vec::new()),
                                                output: (0, 0, Vec::new()),
                                                burn_left: 0,
                                                progress: 0,
                                                dirty: false,
                                                xp: 0.0,
                                                recipe_cache: CachedLookup::default(),
                                                fuel_cache: CachedLookup::default(),
                                            });
                                            let dst: i16 = if smelt_out(st.0).is_some() {
                                                0
                                            } else if fuel_ticks(st.0) > 0 {
                                                1
                                            } else {
                                                -1
                                            };
                                            if dst >= 0 {
                                                let m = if dst == 0 {
                                                    &mut f.input
                                                } else {
                                                    &mut f.fuel
                                                };
                                                let fits = m.1 == 0
                                                    || (m.0 == st.0
                                                        && m.2 == st.2
                                                        && (m.1 as i64)
                                                            < item_stack_max(m.0, &m.2));
                                                if fits {
                                                    inv.remove(&(slot + 6));
                                                    let room = item_stack_max(st.0, &st.2)
                                                        .min(255)
                                                        as u8;
                                                    let t = if m.1 == 0 {
                                                        room.min(st.1)
                                                    } else {
                                                        (room - m.1).min(st.1)
                                                    };
                                                    if m.1 == 0 {
                                                        *m = (st.0, t, st.2.clone());
                                                    } else {
                                                        m.1 += t;
                                                    }
                                                    f.dirty = true;
                                                    if t < st.1 {
                                                        inv.insert(
                                                            slot + 6,
                                                            (st.0, st.1 - t, st.2),
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    let cur_st = if slot >= 3 {
                                        inv.get(&(slot + 6)).cloned().unwrap_or((0, 0, Vec::new()))
                                    } else {
                                        let f2 = fs.get(&fp).unwrap();
                                        match slot {
                                            0 => f2.input.clone(),
                                            1 => f2.fuel.clone(),
                                            _ => f2.output.clone(),
                                        }
                                    };
                                    resync_slot(&me, 2, slot, &cur_st);
                                }
                                continue;
                            }
                            // inventory window: 5-8 armor | 45 offhand | 1-4 craft | 9-44 inv
                            {
                                let mut inv = me.inv.lock().unwrap();
                                let mut grid = me.craft.lock().unwrap();
                                if mode == 4 {
                                    let got = if (5..=8).contains(&slot)
                                        || (9..=44).contains(&slot)
                                        || slot == 45
                                    {
                                        map_take(&mut inv, slot, if btn == 1 { 255 } else { 1 })
                                    } else if (1..=4).contains(&slot) {
                                        map_take(&mut grid, slot, if btn == 1 { 255 } else { 1 })
                                    } else {
                                        None
                                    };
                                    if let Some(st) = got {
                                        throw!(st);
                                    }
                                    resync_slot(
                                        &me,
                                        0,
                                        slot,
                                        &inv.get(&slot).cloned().unwrap_or((0, 0, Vec::new())),
                                    );
                                } else if (9..=35).contains(&slot) {
                                    // shift from main inv: armor equips, else -> hotbar
                                    if let Some(st) = inv.get(&slot).cloned() {
                                        let asl = armor_slot_for(st.0);
                                        if asl >= 0 && !inv.contains_key(&asl) {
                                            inv.remove(&slot);
                                            inv.insert(asl, st);
                                            resync_slot(
                                                &me,
                                                0,
                                                asl,
                                                &inv.get(&asl).cloned().unwrap(),
                                            );
                                        } else {
                                            inv.remove(&slot);
                                            let rem = move_to_slots(&mut inv, 36..=44, st);
                                            if rem.1 > 0 {
                                                inv.insert(slot, rem);
                                            }
                                        }
                                    }
                                } else if (36..=44).contains(&slot) {
                                    if let Some(st) = inv.get(&slot).cloned() {
                                        inv.remove(&slot);
                                        let rem = move_to_slots(&mut inv, 9..=35, st);
                                        if rem.1 > 0 {
                                            inv.insert(slot, rem);
                                        }
                                    }
                                } else if (5..=8).contains(&slot) || slot == 45 {
                                    // unequip into inventory
                                    if let Some(st) = inv.get(&slot).cloned() {
                                        inv.remove(&slot);
                                        let rem = move_to_slots(&mut inv, 9..=44, st);
                                        if rem.1 > 0 {
                                            inv.insert(slot, rem);
                                        }
                                    }
                                } else if (1..=4).contains(&slot) {
                                    if let Some(st) = grid.get(&slot).cloned() {
                                        grid.remove(&slot);
                                        let rem = move_to_slots(&mut inv, 9..=44, st);
                                        if rem.1 > 0 {
                                            grid.insert(slot, rem);
                                        }
                                    }
                                }
                                *me.armor_pts.lock().unwrap() = armor_pts_of(&inv);
                                drop(grid);
                                drop(inv);
                                resync_slot(&me, 0, slot, &{
                                    let i2 = me.inv.lock().unwrap();
                                    i2.get(&slot).cloned().unwrap_or((0, 0, Vec::new()))
                                });
                                // armor/offhand moves need armor slot echo too
                                if (5..=8).contains(&slot) || slot == 45 || mode == 1 {
                                    for asl in 5..=8i16 {
                                        let st2 =
                                            me.inv.lock().unwrap().get(&asl).cloned().unwrap_or((
                                                0,
                                                0,
                                                Vec::new(),
                                            ));
                                        resync_slot(&me, 0, asl, &st2);
                                    }
                                }
                            }
                        }
                    }
                }
            _ => {}
        }
        apply_queued_block_updates();
    }
    {
        eprintln!("[spine] {} left (from {})", name, peer);
        {
            // remove owned projectile/bobber entities
            let mut fb = bobbers().lock().unwrap();
            let gone_b: Vec<i32> = fb
                .iter()
                .filter(|b| b.owner == me_cleanup.eid)
                .map(|b| b.eid)
                .collect();
            fb.retain(|b| b.owner != me_cleanup.eid);
            let mut pq = pearls().lock().unwrap();
            let gone_p: Vec<i32> = pq
                .iter()
                .filter(|p| p.owner == me_cleanup.eid)
                .map(|p| p.eid)
                .collect();
            pq.retain(|p| p.owner != me_cleanup.eid);
            for eid in gone_b.iter().chain(gone_p.iter()) {
                broadcast_seen_destroy(*eid);
            }
        }
        drop_player_items_on_death(&me_cleanup);
        close_live_menu(&me_cleanup, false, false);
        {
            // vanilla containerMenu.removed: cursor and crafting grid
            // are placed back into the inventory (the
            // overflow falls on the ground as item entities)
            let mut back: Vec<Stack> = Vec::new();
            let cur = std::mem::take(&mut *me_cleanup.cursor.lock().unwrap());
            if cur.1 > 0 {
                back.push(cur);
            }
            for g in [&me_cleanup.craft, &me_cleanup.craft9] {
                for (_, st) in std::mem::take(&mut *g.lock().unwrap()) {
                    if st.1 > 0 {
                        back.push(st);
                    }
                }
            }
            let mut inv = me_cleanup.inv.lock().unwrap();
            let mut to_drop: Vec<Stack> = Vec::new();
            for st in back {
                // merge into matching stacks first, then first empty slot
                let mut left = st.1 as i64;
                let cap = item_stack_max(st.0, &st.2);
                for v in inv.values_mut() {
                    if left <= 0 {
                        break;
                    }
                    if v.0 == st.0 && v.2 == st.2 && (v.1 as i64) < cap {
                        let t = (cap - v.1 as i64).min(left);
                        v.1 += t as u8;
                        left -= t;
                    }
                }
                while left > 0 {
                    if let Some(sl) = (9..=44).chain(36..=44).find(|sl| !inv.contains_key(sl)) {
                        let t = left.min(cap) as u8;
                        inv.insert(sl, (st.0, t, st.2.clone()));
                        left -= t as i64;
                    } else {
                        to_drop.push((st.0, left.min(cap) as u8, st.2.clone()));
                        left -= cap.min(left);
                    }
                }
            }
            if !to_drop.is_empty() {
                let (px, py, pz, ..) = *me_cleanup.pos.lock().unwrap();
                let mut drp = drops().lock().unwrap();
                for st in to_drop {
                    drp.push(Drop {
                        eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                        x: px,
                        y: py + 1.3,
                        z: pz,
                        item: st.0,
                        cnt: st.1,
                        comps: st.2,
                        age: 0,
                        target: None,
                        thrower: None,
                    });
                }
            }
            *me_cleanup.drag.lock().unwrap() = (-1, Vec::new());
            let inventory_snapshot = inv.clone();
            drop(inv);
            inv_store()
                .lock()
                .unwrap()
                .insert(name.clone(), inventory_snapshot.clone());
            // A logout persists the dimension the player was actually in, so the next
            // login's 0x2b Join Game and first chunk window resume in that world.
            let record = client_dim_pos(&me_cleanup);
            pos_store().lock().unwrap().insert(name.clone(), record);
            save_live_player_snapshot(&me_cleanup, record, &inventory_snapshot);
        }
        // vanilla `ServerPlayer.disconnect` -> `containerMenu.removed` -> stopOpen:
        // the leaving viewer stops counting before it leaves the client list. The window
        // is read into a local first: `if let Some(open) = *mutex.lock().unwrap()` holds
        // the lock for the whole body, and the body locks it again to clear it.
        if let Some((open, anchor)) = client_open_window_and_anchor(&me_cleanup) {
            let audience = client_snapshot();
            let cdim = *me_cleanup.dim.lock().unwrap_or_else(|e| e.into_inner());
            let halves = client_window_halves_dim(open, anchor, cdim);
            let before = chest_opener_snapshot_dim(&audience, &halves);
            *me_cleanup.open_chest.lock().unwrap() = None;
            *me_cleanup.chest_sync.lock().unwrap() = None;
            sync_chest_openers_dim(&audience, &before);
        }
        #[cfg(feature = "neoforge-events")]
        if java_compat::NEOFORGE.get().is_some() {
            let _ = neoforge_event(
                &me_cleanup,
                java_compat::S1Event::PlayerLogout(me_cleanup.eid, me_cleanup.name.clone()),
            );
        }
        let mut cls = clients().lock().unwrap();
        cls.retain(|c| c.eid != me_cleanup.eid);
        drop(cls);
        *me_cleanup.sleeping.lock().unwrap() = None;
        player_mechanism_sessions().lock().unwrap().remove(&me_cleanup.eid);
        death_drop_marks().lock().unwrap().remove(&me_cleanup.eid);
        beacon_menu_anchors().lock().unwrap().remove(&me_cleanup.eid);
        // A disconnecting player drops both sides of `ServerPlayer.camera`: the
        // entry naming their own view and any spectator still watching them.
        {
            let mut cams = spectator_cameras().lock().unwrap_or_else(|e| e.into_inner());
            cams.remove(&me_cleanup.eid);
            cams.retain(|_, target| *target != me_cleanup.eid);
        }
        // `Entity.remove` on the leaving player: they step out of any vehicle
        // and the vehicle's trackers get the updated SetPassengers.
        live_mount_forget(me_cleanup.eid);
        // vanilla culls the destroy to this entity's trackers
        broadcast_seen_destroy(me_cleanup.eid);
        let mut p = Vec::new();
        write_varint(&mut p, 1);
        p.extend_from_slice(&me_cleanup.uuid);
        broadcast(me_cleanup.eid, |_c| (0x3d, p.clone()));
    }
}

/// The `server-icon.png` favicon for the status reply, loaded once at first
/// ping. An absent or invalid icon is simply omitted — a bad file must never
/// take down a status answer.
fn status_favicon() -> Option<&'static str> {
    static FAVICON: OnceLock<Option<String>> = OnceLock::new();
    FAVICON
        .get_or_init(|| {
            match load_server_icon_favicon(std::path::Path::new("server-icon.png")) {
                Ok(icon) => icon,
                Err(why) => {
                    eprintln!("[spine] {why}");
                    None
                }
            }
        })
        .as_deref()
}

fn status_version_mismatch(s: &mut TcpStream, reason: &str, deadline: Instant) {
    if !matches!(read_packet(s, deadline), Some((0, _))) {
        return;
    }
    // Status has no disconnect packet; report the incompatibility in the
    // server-list description using the normal status response shape.
    let json = format!("{{\"version\":{{\"name\":\"1.21.1\",\"protocol\":767}},\"players\":{{\"max\":0,\"online\":0}},\"description\":{{\"text\":\"{reason}\"}}}}");
    let mut pl = Vec::new();
    write_str(&mut pl, &json);
    let _ = send_packet(s, 0, pl);
    if let Some((1, ping)) = read_packet(s, deadline) {
        let _ = send_packet(s, 1, ping);
    }
}

// A panic in a connection handler must not leak a permanently reserved slot.
struct ConnectionSlot<'a>(&'a AtomicUsize);
impl std::ops::Drop for ConnectionSlot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
static IP_CONNS: connection_limits::IpConnections = connection_limits::IpConnections::new();
static LOGINS: connection_limits::LoginRegistry = connection_limits::LoginRegistry::new();

// ---------------- Live pet interact: collar dyes + parrot taming ----------------
// Wolf/cat tame + sitting already ride `live_mob_interact`; this front half
// covers what it does not know: vanilla dyes on a player-owned collar, and the
// parrot, whose tame items `PetBehavior::try_tame` does not name. Parrot tame
// state is kept in `parrot_pets`: `MobBehaviorState` has no parrot variant, and
// tagging one as Cat would write fields `save_v2`'s reader cannot consume back
// for etype 75.
fn parrot_pets() -> &'static Mutex<HashMap<i32, PetBehavior>> {
    static P: OnceLock<Mutex<HashMap<i32, PetBehavior>>> = OnceLock::new();
    P.get_or_init(Mutex::default)
}

// Vanilla dye order matches the item ids: white_dye (944) .. black_dye (959)
// -> collar colors 0..=15.
fn pet_dye_color(item: i64) -> Option<u8> {
    (944..=959).contains(&item).then_some((item - 944) as u8)
}

fn parrot_seed(item: i64) -> bool {
    // wheat_seeds | pumpkin_seeds | melon_seeds | torchflower_seeds | beetroot_seeds
    matches!(item, 853 | 986 | 987 | 1152 | 1155)
}

fn consume_one_item(client: &Client, slot: i16) {
    if *client.gm.lock().unwrap_or_else(|e| e.into_inner()) == 1 {
        return;
    }
    let mut inv = client.inv.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(stack) = inv.get_mut(&slot) {
        stack.1 -= 1;
        if stack.1 == 0 {
            inv.remove(&slot);
        }
    }
    let updated = inv.get(&slot).cloned().unwrap_or((0, 0, Vec::new()));
    drop(inv);
    resync_slot(client, 0, slot, &updated);
}

enum PetInteract {
    Collar { etype: i64, color: u8 },
    ParrotFed { tamed: bool, sitting: bool },
    Sitting(bool),
}

fn pet_collar_or_parrot_interact(client: &Client, target: i32, slot: i16) -> bool {
    if *client.gm.lock().unwrap_or_else(|e| e.into_inner()) == 3
        || !client
            .seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&target)
    {
        return false;
    }
    let mob = mobs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|m| m.eid == target && matches!(m.etype, 15 | 75 | 122))
        .cloned();
    let Some(mob) = mob else { return false };
    let item = client
        .inv
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&slot)
        .map(|stack| stack.0)
        .unwrap_or(0);
    let mut effect = None;
    let mut consume = false;
    if mob.etype == 75 {
        let mut pets = parrot_pets().lock().unwrap_or_else(|e| e.into_inner());
        let pet = pets
            .entry(target)
            .or_insert_with(|| PetBehavior::new(PetKind::Cat));
        if pet.owner.is_none() {
            if parrot_seed(item) {
                // vanilla Parrot.mobInteract: each seed is eaten, taming lands
                // one roll in ten.
                consume = true;
                if world_rng::next_u64() % 10 == 0 {
                    pet.owner = Some(client.uuid);
                    pet.sitting = true;
                    pet.hp = *mob.hp.lock().unwrap_or_else(|e| e.into_inner());
                }
                effect = Some(PetInteract::ParrotFed {
                    tamed: pet.owner.is_some(),
                    sitting: pet.sitting,
                });
            }
        } else if pet.owner == Some(client.uuid) {
            pet.sitting = !pet.sitting;
            effect = Some(PetInteract::Sitting(pet.sitting));
        }
    } else if let Some(color) = pet_dye_color(item) {
        let mut states = live_mob_states().lock().unwrap_or_else(|e| e.into_inner());
        let state = states
            .entry(target)
            .or_insert_with(|| LiveMobState::new(mob.etype));
        if let Some(MobBehaviorState::Wolf(pet) | MobBehaviorState::Cat(pet)) =
            &mut state.behavior
        {
            if pet.dye_collar(client.uuid, color) {
                consume = true;
                effect = Some(PetInteract::Collar {
                    etype: mob.etype,
                    color,
                });
            }
        }
    }
    if !consume && effect.is_none() {
        return false;
    }
    if consume {
        consume_one_item(client, slot);
    }
    match effect {
        // Collar metadata is an Int serializer: wolf at index 20 (1.21.5 slots
        // DATA_SOUND_VARIANT at 19), cat at index 21.
        Some(PetInteract::Collar { etype, color }) => {
            let index = if etype == 122 { 20 } else { 21 };
            broadcast_seen(target, |_| {
                (0x58, live_mob_metadata(target, index, 1, color))
            });
        }
        Some(PetInteract::ParrotFed { tamed, sitting }) => {
            let flags = u8::from(sitting) | (u8::from(tamed) << 2);
            broadcast_seen(target, |_| (0x58, live_mob_metadata(target, 17, 0, flags)));
            let mut packet = target.to_be_bytes().to_vec();
            packet.push(if tamed { 7 } else { 6 });
            broadcast_seen(target, |_| (0x1f, packet.clone()));
        }
        Some(PetInteract::Sitting(sitting)) => {
            let flags = u8::from(sitting) | 4;
            broadcast_seen(target, |_| (0x58, live_mob_metadata(target, 17, 0, flags)));
        }
        None => {}
    }
    true
}

/// Vanilla `TamableAnimal` follow teleport: a standing tamed pet more than 12
/// blocks out (`dist_sq > 144.0`) reseats on the nearest safe, solid floor next
/// to its owner. Sitting pets never teleport, and a landing only counts when
/// the block below is solid while the two blocks at foot and head height are
/// not.
fn pet_follow_teleport_step(
    owner_dim: DimId,
    owner_pos: (f64, f64, f64),
    pet_pos: (f64, f64, f64),
    sitting: bool,
    mut is_solid_floor: impl FnMut(DimId, i32, i32, i32) -> bool,
) -> Option<(f64, f64, f64)> {
    if sitting {
        return None;
    }
    let (dx, dy, dz) = (
        owner_pos.0 - pet_pos.0,
        owner_pos.1 - pet_pos.1,
        owner_pos.2 - pet_pos.2,
    );
    if dx * dx + dy * dy + dz * dz <= 144.0 {
        return None;
    }
    let (ox, oy, oz) = (
        owner_pos.0.floor() as i32,
        owner_pos.1.floor() as i32,
        owner_pos.2.floor() as i32,
    );
    for dy in -1..=1i32 {
        for dx in -3..=3i32 {
            for dz in -3..=3i32 {
                let (bx, by, bz) = (ox + dx, oy + dy, oz + dz);
                if is_solid_floor(owner_dim, bx, by - 1, bz)
                    && !is_solid_floor(owner_dim, bx, by, bz)
                    && !is_solid_floor(owner_dim, bx, by + 1, bz)
                {
                    return Some((bx as f64 + 0.5, by as f64, bz as f64 + 0.5));
                }
            }
        }
    }
    None
}

// ---------------- Lane P8a: dimension-scoped interact helpers ----------------
// The legacy interact helpers in `menus_and_items.rs` read `block_at` and key
// containers with `ContainerKey::ow` — Overworld only. These variants thread
// the interacting player's dimension through every world read and container
// key, so a chest clicked in the Nether resolves Nether blocks and Nether
// storage.

/// `chest_window_halves` that keeps each half's dimension from its key.
fn chest_window_halves_dim(open: (ContainerKey, Option<ContainerKey>)) -> Vec<DimBlockPos> {
    let mut out: Vec<DimBlockPos> = Vec::new();
    for key in [Some(open.0), open.1].into_iter().flatten() {
        if let ContainerKey::Block((dim, x, y, z)) = key {
            if !is_ender_key((x, y, z)) && !out.contains(&(dim, x, y, z)) {
                out.push((dim, x, y, z));
            }
        }
    }
    out
}
/// `client_window_halves` keeping dimensions: an ender window exposes only its
/// anchor half, which lives in the clicking client's dimension.
fn client_window_halves_dim(
    open: (ContainerKey, Option<ContainerKey>),
    anchor: Option<BlockPos>,
    client_dim: DimId,
) -> Vec<DimBlockPos> {
    if matches!(open, (ContainerKey::Block(ek), None) if is_ender_key((ek.1, ek.2, ek.3))) {
        return anchor
            .into_iter()
            .map(|p| (client_dim, p.0, p.1, p.2))
            .collect();
    }
    chest_window_halves_dim(open)
}
/// `ChestBlock.isBlockedChestByBlock` in `dim`: true when `pos.above()` is a
/// redstone conductor in that dimension.
fn is_blocked_chest_by_block_dim(dim: DimId, pos: BlockPos) -> bool {
    if dim == DimId::Overworld {
        return is_blocked_chest_by_block(pos);
    }
    let above = block_at_dim(dim, pos.0, pos.1 + 1, pos.2);
    breg().is_some_and(|reg| reg.is_redstone_conductor(above))
}
/// `ChestBlock.isChestBlockedAt` in `dim`.
fn is_chest_blocked_at_dim(dim: DimId, pos: BlockPos) -> bool {
    is_blocked_chest_by_block_dim(dim, pos) || is_cat_sitting_on_chest(pos)
}
/// `ChestBlock.getMenuProvider` in `dim`: a single chest is blocked when its
/// cell is, a double chest when EITHER half is.
fn chest_window_blocked_dim(dim: DimId, open: (ContainerKey, Option<ContainerKey>)) -> bool {
    if dim == DimId::Overworld {
        return chest_window_blocked(open);
    }
    chest_window_halves(open)
        .into_iter()
        .any(|pos| is_chest_blocked_at_dim(dim, pos))
}
/// `half_openers` in `dim`: openers whose window carries this dimensioned block
/// key, plus the ender-chest anchor match.
fn half_openers_dim(dim: DimId, audience: &[Arc<Client>], pos: BlockPos) -> u32 {
    let key = ContainerKey::Block((dim, pos.0, pos.1, pos.2));
    audience
        .iter()
        .filter(|c| {
            if c.is_spectator() {
                return false;
            }
            let open = match c.open_chest.lock() {
                Ok(g) => *g,
                Err(_) => return false,
            };
            let Some((a, b)) = open else {
                return false;
            };
            if a == key || b == Some(key) {
                return true;
            }
            if matches!(a, ContainerKey::Block(ek) if is_ender_key((ek.1, ek.2, ek.3))) && b.is_none() {
                // An ender window only opens the block in the client's own
                // dimension: same coordinates elsewhere are a different chest.
                if *c.dim.lock().unwrap_or_else(|e| e.into_inner()) != dim {
                    return false;
                }
                return c
                    .chest_sync
                    .lock()
                    .ok()
                    .and_then(|s| {
                        s.as_ref()
                            .filter(|st| st.window == (a, b))
                            .and_then(|st| st.anchor)
                    })
                    == Some(pos);
            }
            false
        })
        .count() as u32
}
/// `chest_opener_snapshot` over dimensioned halves: the count of every half
/// before a window change, so the change can be diffed rather than assumed.
fn chest_opener_snapshot_dim(
    audience: &[Arc<Client>],
    halves: &[DimBlockPos],
) -> Vec<(DimBlockPos, u32)> {
    halves
        .iter()
        .map(|&(d, x, y, z)| ((d, x, y, z), half_openers_dim(d, audience, (x, y, z))))
        .collect()
}
/// The `Level.blockEvent` fanout for one dimension: `Level.blockEvent` is
/// per-level, so a chest's opener-count event never reaches clients in another
/// dimension even when their coordinates are within earshot.
fn broadcast_block_event_dim(
    audience: &[Arc<Client>],
    dim: DimId,
    pos: BlockPos,
    action: u8,
    param: u8,
) {
    let Some(block) =
        breg().and_then(|reg| reg.block_id_of(block_at_dim(dim, pos.0, pos.1, pos.2)))
    else {
        return;
    };
    let mut body = Vec::with_capacity(12);
    write_pos(&mut body, pos.0, pos.1, pos.2);
    body.push(action);
    body.push(param);
    write_varint(&mut body, i64::from(block));
    fanout_in(
        audience,
        |c| {
            if *c.dim.lock().unwrap_or_else(|e| e.into_inner()) != dim {
                return false;
            }
            let at = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
            let (dx, dy, dz) = (
                at.0 - f64::from(pos.0),
                at.1 - f64::from(pos.1),
                at.2 - f64::from(pos.2),
            );
            dx * dx + dy * dy + dz * dz <= 64.0 * 64.0
        },
        |_| (0x08, body.clone()),
    );
}
/// The `0x09` block-update fanout for one dimension, the counterpart of
/// `broadcast_block_event_dim` for the barrel `open` toggle.
fn broadcast_block_update_dim(audience: &[Arc<Client>], dim: DimId, pos: BlockPos, state: u32) {
    let mut body = Vec::with_capacity(12);
    write_pos(&mut body, pos.0, pos.1, pos.2);
    write_varint(&mut body, i64::from(state));
    let chunk = (pos.0 >> 4, pos.2 >> 4);
    fanout_in(
        audience,
        |c| {
            if *c.dim.lock().unwrap_or_else(|e| e.into_inner()) != dim {
                return false;
            }
            if c.chunks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&chunk)
            {
                return true;
            }
            let at = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
            let (dx, dy, dz) = (
                at.0 - f64::from(pos.0),
                at.1 - f64::from(pos.1),
                at.2 - f64::from(pos.2),
            );
            dx * dx + dy * dy + dz * dz <= 64.0 * 64.0
        },
        |_| (0x09, body.clone()),
    );
}
/// `sync_chest_openers` over dimensioned halves: each half's block reads and
/// `openedBy` writes stay inside the half's own dimension.
fn sync_chest_openers_dim(audience: &[Arc<Client>], before: &[(DimBlockPos, u32)]) {
    for &((dim, x, y, z), previous) in before {
        let pos = (x, y, z);
        let now = half_openers_dim(dim, audience, pos);
        if now != previous {
            let st = block_at_dim(dim, x, y, z);
            if let Some(reg) = breg().filter(|reg| reg.name_of(st) == "minecraft:barrel") {
                if (previous == 0) != (now == 0) {
                    let next = reg.with_prop(st, "open", if now > 0 { "true" } else { "false" });
                    if next != st {
                        set_block_dim(dim, x, y, z, next);
                        broadcast_block_update_dim(audience, dim, pos, next);
                    }
                }
            } else {
                // vanilla writes the count with `writeByte`: the wire field is a byte.
                broadcast_block_event_dim(audience, dim, pos, 1, now as u8);
            }
        }
    }
}
/// `chest_window` in `dim`: the Overworld keeps the legacy helper, other
/// dimensions resolve through the dimension-aware twin.
fn chest_window_for_player(
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
) -> (ContainerKey, Option<ContainerKey>) {
    if dim == DimId::Overworld {
        chest_window(x, y, z)
    } else {
        chest_window_dim(dim, x, y, z)
    }
}
/// `ChestBlock.candidatePartnerFacing` in `dim`.
fn chest_candidate_partner_facing_dim(dim: DimId, pos: BlockPos, name: &str) -> Option<HFace> {
    if dim == DimId::Overworld {
        return chest_candidate_partner_facing(pos, name);
    }
    let st = block_at_dim(dim, pos.0, pos.1, pos.2);
    if breg()?.name_of(st) != name {
        return None;
    }
    if chest_type(st)? != "single" {
        return None;
    }
    chest_facing(st)
}
/// `ChestBlock.getStateForPlacement` in `dim`: the same facing and pairing
/// rules, but every neighbour read lands in the placing player's dimension.
fn chest_placement_state_dim(
    dim: DimId,
    name: &str,
    pos: BlockPos,
    yaw: f32,
    sneaking: bool,
    face: u8,
) -> u32 {
    if dim == DimId::Overworld {
        return chest_placement_state(name, pos, yaw, sneaking, face);
    }
    let Some(reg) = breg() else { return 0 };
    let mut facing = HFace::from_yaw(yaw).opposite();
    let mut kind = "single";
    // Sneaking against a horizontal face claims that side: only the clicked
    // block may become the partner, and only when its facing is on the other axis.
    if sneaking {
        if let Some(clicked) = HFace::from_face(face) {
            let back = clicked.opposite();
            let (bx, bz) = back.step();
            let behind = (pos.0 + bx, pos.1, pos.2 + bz);
            if let Some(candidate) = chest_candidate_partner_facing_dim(dim, behind, name) {
                if candidate.axis() != clicked.axis() {
                    facing = candidate;
                    kind = if candidate.counter_clock_wise() == back {
                        "right"
                    } else {
                        "left"
                    };
                }
            }
        }
    }
    // Otherwise the pair axis is perpendicular to facing: clockwise is LEFT,
    // then counter-clockwise is RIGHT (vanilla order, first match wins).
    if kind == "single" && !sneaking {
        let (cx, cz) = facing.clock_wise().step();
        let clockwise = (pos.0 + cx, pos.1, pos.2 + cz);
        if chest_candidate_partner_facing_dim(dim, clockwise, name) == Some(facing) {
            kind = "left";
        } else {
            let (cx, cz) = facing.counter_clock_wise().step();
            let counter = (pos.0 + cx, pos.1, pos.2 + cz);
            if chest_candidate_partner_facing_dim(dim, counter, name) == Some(facing) {
                kind = "right";
            }
        }
    }
    let waterlogged =
        reg.fluid_name(block_at_dim(dim, pos.0, pos.1, pos.2)) == "minecraft:water";
    let st = reg.state_of(name, None);
    let st = reg.with_prop(st, "facing", facing.name());
    let st = reg.with_prop(st, "type", kind);
    reg.with_prop(st, "waterlogged", if waterlogged { "true" } else { "false" })
}
/// `ServerboundResourcePackPacket` statuses that end the connection when the
/// pack is required (`handleResourcePack` disconnects on Declined,
/// FailedDownload, InvalidUrl and FailedReload). Optional packs — and
/// successful or progress statuses — never disconnect.
fn resource_pack_status_disconnects(status: Option<i64>) -> bool {
    resource_pack_required() && matches!(status, Some(1 | 2 | 5 | 6))
}

#[cfg(test)]
mod connection_slot_tests {
    use super::*;
    #[test]
    fn panic_does_not_leak_connection_slot() {
        let count = AtomicUsize::new(1);
        let result = std::panic::catch_unwind(|| {
            let _slot = ConnectionSlot(&count);
            panic!("synthetic handler panic");
        });
        assert!(result.is_err());
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }
}

#[cfg(test)]
mod handshake_security_tests {
    use super::*;
    #[test]
    fn valid_handshake_and_login_identity() {
        let mut h = Vec::new();
        write_varint(&mut h, 767);
        write_str(&mut h, "localhost");
        h.extend_from_slice(&25565u16.to_be_bytes());
        write_varint(&mut h, 2);
        assert_eq!(handshake_parts(&h), Some((767, 2)));
        let mut login = Vec::new();
        write_str(&mut login, "Test_Player");
        login.extend_from_slice(&[7u8; 16]);
        let expected = [
            0xa3, 0x3a, 0xc9, 0xaa, 0x13, 0xa8, 0x30, 0x09, 0xa0, 0x68, 0xbb, 0x13, 0x80,
            0xa2, 0xd0, 0xb0,
        ];
        assert_eq!(login_identity(&login), Some(("Test_Player".into(), expected)));
    }
    #[test]
    fn malformed_handshake_is_rejected_without_panicking() {
        let mut h = Vec::new();
        write_varint(&mut h, 767);
        write_varint(&mut h, i64::MAX);
        assert_eq!(handshake_parts(&h), None);
        let mut truncated = Vec::new();
        write_varint(&mut truncated, 767);
        write_str(&mut truncated, "localhost");
        truncated.extend_from_slice(&25565u16.to_be_bytes());
        assert_eq!(handshake_parts(&truncated), None);
        let mut bad_proto = Vec::new();
        write_varint(&mut bad_proto, 766);
        write_str(&mut bad_proto, "localhost");
        bad_proto.extend_from_slice(&25565u16.to_be_bytes());
        write_varint(&mut bad_proto, 2);
        assert_eq!(handshake_parts(&bad_proto), Some((766, 2)));
    }

    #[test]
    fn status_and_login_handshakes_preserve_unsupported_protocol() {
        for next in [1, 2] {
            let mut hs = Vec::new();
            write_varint(&mut hs, 766);
            write_str(&mut hs, "localhost");
            hs.extend_from_slice(&25565u16.to_be_bytes());
            write_varint(&mut hs, next);
            assert_eq!(handshake_parts(&hs), Some((766, next)));
        }
    }
    #[test]
    fn malformed_login_does_not_assume_zero_uuid() {
        let mut short = Vec::new();
        write_str(&mut short, "Alice");
        assert!(login_identity(&short).is_none());
        let mut long_name = Vec::new();
        write_str(&mut long_name, "APlayerWithAVeryLongUsername");
        long_name.extend_from_slice(&[1u8; 16]);
        assert!(login_identity(&long_name).is_none());
    }
}

#[cfg(test)]
mod skin_cache_tests {
    use super::*;

    fn skin(n: u8) -> Option<(String, String)> {
        Some((format!("v{n}"), format!("s{n}")))
    }

    #[test]
    fn the_cache_never_exceeds_its_cap_and_evicts_the_oldest() {
        let mut c = HashMap::new();
        let t0 = Instant::now();
        for i in 0..10u8 {
            skin_cache_put(
                &mut c,
                format!("n{i}"),
                skin(i),
                t0 + Duration::from_secs(i as u64),
                4,
            );
            assert!(c.len() <= 4);
        }
        assert!(skin_cache_get(&c, "n9", t0 + Duration::from_secs(9)).is_some());
        assert!(skin_cache_get(&c, "n0", t0 + Duration::from_secs(9)).is_none());
        // re-inserting an existing key at the cap does not evict anything
        let before = c.len();
        skin_cache_put(
            &mut c,
            "n9".into(),
            skin(99),
            t0 + Duration::from_secs(10),
            4,
        );
        assert_eq!(c.len(), before);
    }

    #[test]
    fn entries_expire_and_misses_expire_sooner_than_hits() {
        let mut c = HashMap::new();
        let t0 = Instant::now();
        skin_cache_put(&mut c, "hit".into(), skin(1), t0, 8);
        skin_cache_put(&mut c, "miss".into(), None, t0, 8);
        let soon = t0 + SKIN_TTL_MISS + Duration::from_secs(1);
        assert_eq!(skin_cache_get(&c, "miss", t0), Some(None));
        assert!(skin_cache_get(&c, "miss", soon).is_none());
        assert_eq!(skin_cache_get(&c, "hit", soon), Some(skin(1)));
        assert!(skin_cache_get(&c, "hit", t0 + SKIN_TTL_HIT + Duration::from_secs(1)).is_none());
        // expired entries are dropped before live ones when making room
        skin_cache_put(&mut c, "a".into(), skin(2), soon, 2);
        assert!(c.contains_key("hit") && c.contains_key("a") && !c.contains_key("miss"));
    }

    #[test]
    fn lookups_in_flight_are_bounded() {
        let mut held = Vec::new();
        while let Some(slot) = SkinLookupSlot::try_acquire() {
            held.push(slot);
            assert!(held.len() <= SKIN_LOOKUPS_MAX);
        }
        assert_eq!(held.len(), SKIN_LOOKUPS_MAX);
        drop(held);
        assert!(SkinLookupSlot::try_acquire().is_some());
    }
}

#[cfg(test)]
mod connection_limits_wire_tests {
    use super::*;

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(2)
    }

    #[test]
    fn login_negotiation_and_play_framing_use_the_same_threshold() {
        for threshold in [-1, 0, 256] {
            let (client, mut peer) = effect_tests::client_pair();
            let mut login_socket = client.sock.lock().unwrap().try_clone().unwrap();
            let registration = negotiate_compression(&mut login_socket, threshold).unwrap();
            let compression = usize::try_from(threshold).ok();
            if compression.is_some() {
                assert_eq!(
                    read_packet(&mut peer, deadline()),
                    Some((3, connection_limits::set_compression(threshold)))
                );
            }
            send_packet(&mut login_socket, 2, vec![42]).unwrap();
            assert_eq!(
                read_login_frame(&mut peer, deadline(), compression),
                Some((2, vec![42]))
            );
            drop(registration); // Login framing and the Play writer have separate owners.

            client.enter_play_wire(compression, Default::default());
            let mut rx = RxBuf::new(compression);
            for payload in [vec![0x12], vec![0x34; 512]] {
                send_packet(&mut client.sock.lock().unwrap(), 0x26, payload.clone()).unwrap();
                let end = deadline();
                loop {
                    assert!(Instant::now() < end, "Play frame did not arrive");
                    match rx.next(&mut peer) {
                        RxOut::Frame(id, body) => {
                            assert_eq!((id, body), (0x26, payload));
                            break;
                        }
                        RxOut::Idle => {}
                        RxOut::Closed => panic!("Play frame failed to decode"),
                    }
                }
            }
        }
    }

    #[test]
    fn tracking_spawn_delta_and_removal_follow_the_observers_loaded_chunks() {
        let (client, mut peer) = effect_tests::client_pair();
        let client = Arc::new(client);
        client.enter_play_wire(None, Default::default());
        let eid = 77;
        let x = 16.0;
        let y = 70.0;
        let z = 0.0;
        let arrow = TrackedSpawn {
            dim: DimId::Overworld,
            range: entity_tracking_range(4),
            job: (
                eid,
                x,
                y,
                z,
                Box::new(move || vec![(0x01, spawn_typed_pkt(eid, 4, x, y, z))]),
            ),
        };
        let entities = [arrow];
        sync_entity_interest(&client, &entities);
        assert!(!client.seen.lock().unwrap().contains(&eid));
        client.chunks.lock().unwrap().insert((1, 0));
        sync_entity_interest(&client, &entities);
        assert_eq!(
            read_packet(&mut peer, deadline()),
            Some((0x01, spawn_typed_pkt(eid, 4, x, y, z)))
        );
        assert!(client.seen.lock().unwrap().contains(&eid));

        let upper = x + f64::from(i16::MAX) / 4096.0;
        let next = upper + 1.5;
        let moves = [
            (upper, 0.0, 0x2e, vec![77, 0x7f, 0xff, 0, 0, 0, 0, 1]),
            (next, 90.0, 0x2f, vec![77, 0x18, 0, 0, 0, 0, 0, 64, 0, 1]),
            (next, 180.0, 0x30, vec![77, 128, 0, 1]),
            (
                next + 8.0,
                180.0,
                0x70,
                entity_teleport(eid, next + 8.0, y, z, 180.0, 0.0, true),
            ),
        ];
        for (position, yaw, id, expected) in moves {
            broadcast_seen_ex_in(std::slice::from_ref(&client), eid, 0, |_| {
                (0x70, entity_teleport(eid, position, y, z, yaw, 0.0, true))
            });
            assert_eq!(read_packet(&mut peer, deadline()), Some((id, expected)));
        }
        broadcast_seen_ex_in(std::slice::from_ref(&client), eid, 0, |_| {
            (0x48, head_rot(eid, 90.0))
        });
        assert_eq!(
            read_packet(&mut peer, deadline()),
            Some((0x48, vec![77, 64]))
        );
        assert!(entity_in_tracking_range((0.0, 0.0), (64.0, 0.0), 4, 8));
        assert!(!entity_in_tracking_range((0.0, 0.0), (64.0, 64.0), 4, 8));
        assert!(!entity_in_tracking_range((0.0, 0.0), (64.01, 0.0), 4, 8));
        assert!(!entity_in_tracking_range((0.0, 0.0), (33.0, 0.0), 4, 2));
        client.chunks.lock().unwrap().clear();
        sync_entity_interest(&client, &entities);
        assert_eq!(
            read_packet(&mut peer, deadline()),
            Some((0x42, vec![1, 77]))
        );
        assert!(!client.seen.lock().unwrap().contains(&eid));
    }

    #[test]
    fn duplicate_poses_follow_the_entity_interval_and_changed_poses_send_immediately() {
        let mut play = PlayWireState::default();
        let eid = 77;
        play.remember_spawn(&spawn_typed_pkt(eid, 4, 0.0, 70.0, 0.0)); // arrow
        let update = |play: &mut PlayWireState, tick, x| {
            let mut id = 0x70;
            let mut payload = entity_teleport(eid, x, 70.0, 0.0, 0.0, 0.0, false);
            play.update_packet(&mut id, &mut payload, tick)
                .then_some((id, payload))
        };
        assert!(update(&mut play, 10, 1.0).is_some());
        assert_eq!(update(&mut play, 11, 1.0), None);
        assert_eq!(
            update(&mut play, 11, 2.0),
            Some((0x2e, vec![77, 0x10, 0, 0, 0, 0, 0, 0]))
        );
        assert_eq!(update(&mut play, 30, 2.0), None);
        assert!(update(&mut play, 31, 2.0).is_some());
    }

    #[test]
    fn moved_chunk_window_unloads_only_sent_chunks_and_prioritizes_the_look_direction() {
        let previous = ChunkWindow {
            center: (0, 0),
            radius: 2,
        };
        let window = ChunkWindow {
            center: (1, 0),
            radius: 2,
        };
        let mut sent = HashSet::from([(-2, -2), (-2, 2), (1, 1), (2, 0)]);
        assert_eq!(window.leaving(Some(previous), &sent), [(-2, -2), (-2, 2)]);
        sent.retain(|&c| window.contains(c));
        let mut feedback = connection_limits::chunk_feedback(25);
        feedback.acknowledge(&25.0f32.to_be_bytes()).unwrap();
        let south = connection_limits::ChunkDelivery::default()
            .next_batch(window, &sent, 0.0, &feedback, 0);
        assert_eq!(south.len(), 23);
        assert_eq!(south[0], (1, 0));
        assert!(south[1].1 > 0);
        assert_eq!(
            south.iter().copied().collect::<HashSet<_>>().len(),
            south.len()
        );
        assert!(south.iter().all(|c| !sent.contains(c)));
        assert_eq!(window.leaving(Some(window), &sent), []);
        let smaller = ChunkWindow {
            center: (1, 0),
            radius: 0,
        };
        assert_eq!(smaller.leaving(Some(window), &sent).len(), 2);
    }

    #[test]
    fn chunk_light_uses_empty_bits_and_keeps_only_nonzero_section_arrays() {
        for dim in [DimId::Overworld, DimId::Nether] {
            let sky_count = if dim.has_skylight() {
                dim.light_sections()
            } else {
                0
            };
            let mut payload = vec![0x11, 0x22]; // all preceding chunk fields
            for mask in [
                if sky_count == 0 { 0 } else { dim.light_mask() },
                dim.light_mask(),
            ] {
                payload.push(1);
                payload.extend(mask.to_be_bytes());
            }
            payload.extend([0, 0]);
            let block_count = dim.light_sections();
            let mut expected_arrays = [Vec::new(), Vec::new()];
            for (layer, count) in [sky_count, block_count].into_iter().enumerate() {
                write_varint(&mut payload, count as i64);
                for idx in 0..count {
                    write_varint(&mut payload, 2048);
                    let mut section = [0; 2048];
                    if idx == 0 {
                        section.fill(0xff);
                    } else if idx == count - 1 {
                        section[512] = 0x0e;
                    }
                    if idx == 0 || idx == count - 1 {
                        expected_arrays[layer].push(section);
                    }
                    payload.extend(section);
                }
            }
            cull_chunk_light(&mut payload, dim);
            let block_lit = 1 | (1u64 << (block_count - 1));
            let sky_lit = if sky_count == 0 { 0 } else { block_lit };
            let all = dim.light_mask() as u64;
            let expected = [
                sky_lit,
                block_lit,
                if sky_count == 0 { 0 } else { all ^ sky_lit },
                all ^ block_lit,
            ];
            assert_eq!(&payload[..2], &[0x11, 0x22]);
            let mut light = &payload[2..];
            for mask in expected {
                assert_eq!(read_protocol_i32(&mut light), Some(i32::from(mask != 0)));
                if mask != 0 {
                    assert_eq!(light[..8], mask.to_be_bytes());
                    light = &light[8..];
                }
            }
            for arrays in expected_arrays {
                assert_eq!(read_protocol_i32(&mut light), Some(arrays.len() as i32));
                for array in arrays {
                    assert_eq!(read_protocol_i32(&mut light), Some(2048));
                    assert_eq!(light[..2048], array);
                    light = &light[2048..];
                }
            }
            assert!(light.is_empty());
        }
    }

    #[test]
    fn config_sends_datapack_nbt_under_0x07_with_stable_vanilla_ids() {
        let mut config = captured_config_packets().unwrap();
        let damage_id = player_attack_damage_wire_id(&config).unwrap();
        let files = [
            (
                "data/minecraft/damage_type/player_attack.json",
                br#"{"message_id":"changed","scaling":"never","exhaustion":0.1}"#.as_slice(),
            ),
            (
                "data/custom/damage_type/first.json",
                br#"{"message_id":"first","scaling":"never","exhaustion":0.0}"#.as_slice(),
            ),
        ];
        let overlays = build_registry_data_payloads([files]).unwrap();
        assert_eq!(
            overlays,
            registry_tests::build_registry_data_payloads([files]).unwrap()
        );
        merge_registry_overlays(&mut config, &overlays).unwrap();
        assert_eq!(player_attack_damage_wire_id(&config), Some(damage_id));
        let (_, packet) = config
            .iter()
            .find(|(pid, packet)| {
                *pid == 0x07 && config_registry_header(packet).unwrap().0 == "minecraft:damage_type"
            })
            .unwrap();
        let (client, mut peer) = effect_tests::client_pair();
        write_frame(&mut client.sock.lock().unwrap(), packet, Some(256)).unwrap();
        let (id, body) = read_login_frame(&mut peer, deadline(), Some(256)).unwrap();
        assert_eq!(id, 0x07);
        let mut body = body.as_slice();
        assert_eq!(registry_wire_name(&mut body), Some("minecraft:damage_type"));
        let entries = registry_wire_entries(body).unwrap();
        assert_eq!(entries[damage_id as usize].0, "minecraft:player_attack");
        assert!(entries[damage_id as usize]
            .1
            .windows(7)
            .any(|window| window == b"changed"));
        assert_eq!(entries.last().unwrap().0, "custom:first");

        let bundled = load_registry_overlays(std::path::Path::new(DATAPACK_DIRECTORY)).unwrap();
        assert!(!bundled.is_empty());
        let config = load_config_packets();
        let names: Vec<_> = config
            .iter()
            .filter(|(id, _)| *id == 0x07)
            .map(|(_, packet)| config_registry_header(packet).unwrap().0)
            .collect();
        assert_eq!(
            names,
            SYNCED_REGISTRIES.map(|name| format!("minecraft:{name}"))
        );
    }

    /// `broadcast_near_in` is `PlayerList.broadcast(except, x, y, z, radius,
    /// dimension, packet)`: only same-dimension clients strictly inside the
    /// packet's own radius receive it.
    #[test]
    fn broadcast_near_in_culls_out_of_range_and_wrong_dimension_clients() {
        use std::io::Read as _;
        fn arrived(r: &mut TcpStream) -> usize {
            let mut buf = [0u8; 4096];
            let mut total = 0;
            loop {
                match r.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => total += n,
                    Err(_) => break,
                }
            }
            total
        }
        let mk = |eid: i32, x: f64, z: f64| {
            let (mut c, r) = effect_tests::client_pair();
            c.eid = eid;
            *c.pos.lock().unwrap() = (x, 70.0, z, 0.0, 0.0, true);
            r.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
            (Arc::new(c), r)
        };
        let (near, mut r_near) = mk(11, 10.0, 0.0);
        let (mid, mut r_mid) = mk(12, 30.0, 0.0);
        let (other_dim, mut r_dim) = mk(13, 0.0, 0.0);
        *other_dim.dim.lock().unwrap() = DimId::Nether;
        let (edge, mut r_edge) = mk(14, 16.0, 0.0);
        let (far, mut r_far) = mk(15, 40.0, 0.0);
        let audience = [near, mid, other_dim, edge, far];

        // A volume-1.0 sound reaches 16 blocks, strictly inside.
        let snd = sound_pkt_named("entity.item.break", 0.0, 70.0, 0.0);
        broadcast_near_in(DimId::Overworld, &audience, 0.0, 70.0, 0.0, -1, |_| {
            (0x68, snd.clone())
        });
        assert!(arrived(&mut r_near) > 0, "10 blocks is inside 16");
        assert_eq!(arrived(&mut r_mid), 0, "30 blocks is past 16");
        assert_eq!(arrived(&mut r_dim), 0, "another dimension hears nothing");
        assert_eq!(arrived(&mut r_edge), 0, "exactly r is out: the boundary is strict");
        assert_eq!(arrived(&mut r_far), 0, "40 blocks is past 16");

        // A forced (longDistance) particle reaches 512; normal reaches 32.
        let forced = vec![1u8];
        broadcast_near_in(DimId::Overworld, &audience, 0.0, 70.0, 0.0, -1, |_| {
            (0x29, forced.clone())
        });
        assert!(arrived(&mut r_far) > 0, "40 blocks is inside 512");
        for r in [&mut r_near, &mut r_mid, &mut r_dim, &mut r_edge] {
            let _ = arrived(r); // drain so the next broadcast asserts cleanly
        }
        let normal = vec![0u8];
        broadcast_near_in(DimId::Overworld, &audience, 0.0, 70.0, 0.0, -1, |_| {
            (0x29, normal.clone())
        });
        assert!(arrived(&mut r_near) > 0, "10 blocks is inside 32");
        assert!(arrived(&mut r_mid) > 0, "30 blocks is inside 32");
        assert_eq!(arrived(&mut r_far), 0, "40 blocks is past 32");
        let _ = arrived(&mut r_edge); // inside 32, but already pinned above

        // A packet with no vanilla range contract is refused, not sent wide.
        broadcast_near_in(DimId::Overworld, &audience, 0.0, 70.0, 0.0, -1, |_| {
            (0x6c, vec![1, 2, 3])
        });
        assert_eq!(arrived(&mut r_near), 0, "chat carries no positioned range");
    }

    /// Two readers of the same column at the same world revision receive the
    /// same shared frame `Arc`; a `set_block*` write bumps the chunk version,
    /// the revision moves, and the next build replaces the entry.
    #[test]
    fn chunk_packet_frames_are_shared_and_invalidated_by_edits() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        crate::test_support::require_vanilla_data();
        let _guard = WORLD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _gen_guard = crate::worldgen::lock_gen_for_tests();
        assert!(worldgen::init(0, crate::name_to_state), "overworld");
        let dim = DimId::Overworld;
        let (cx, cz) = (0, 0);
        clear_chunk_caches_for_tests();
        chunk_packet_cache().lock().unwrap().clear();
        let mut gen = GenSources::new();
        let f1 = cached_chunk_frame(None, &mut gen, dim, cx, cz, false);
        let mut gen2 = GenSources::new();
        let f2 = cached_chunk_frame(None, &mut gen2, dim, cx, cz, false);
        assert!(
            Arc::ptr_eq(&f1, &f2),
            "a second reader reuses the exact frame Arc"
        );
        assert_eq!(
            gen2.get(&(cx, cz)),
            gen.get(&(cx, cz)),
            "a hit restores the chunk's own emitters like a rebuild"
        );
        {
            let mut body = &f1[..];
            read_protocol_i32(&mut body); // frame length
            assert_eq!(read_protocol_i32(&mut body), Some(0x27));
        }
        // A block edit bumps the 3x3 version tuple; the revision moves.
        let stone = name_to_state("stone").expect("stone in the pinned registry");
        let cur = block_at_dim(dim, 8, 70, 8);
        let st = if cur == stone { 12958 } else { stone };
        set_block_dim(dim, 8, 70, 8, st);
        let f3 = cached_chunk_frame(None, &mut gen, dim, cx, cz, false);
        assert!(!Arc::ptr_eq(&f1, &f3), "the edit must not hit the old frame");
        assert_ne!(f1, f3, "the rebuild carries the edited cell");
        let f4 = cached_chunk_frame(None, &mut GenSources::new(), dim, cx, cz, false);
        assert!(Arc::ptr_eq(&f3, &f4), "the rebuilt frame is shared too");
        // Compressed readers share one lazily compressed frame of the same body.
        let c1 = cached_chunk_frame(Some(256), &mut gen, dim, cx, cz, false);
        let c2 = cached_chunk_frame(Some(256), &mut gen, dim, cx, cz, false);
        assert!(Arc::ptr_eq(&c1, &c2));
        set_block_dim(dim, 8, 70, 8, cur);
        let f5 = cached_chunk_frame(None, &mut gen, dim, cx, cz, false);
        assert_eq!(f1, f5, "restoring the cell restores the bytes");
    }
}

#[cfg(test)]
mod sleep_protocol_parity_tests {
    use super::*;

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(2)
    }

    #[test]
    fn bed_sleep_respects_100_ticks_and_players_sleeping_percentage() {
        let _serial = SLEEP_TEST_LOCK.lock().unwrap();
        let old_clients = std::mem::take(&mut *clients().lock().unwrap());
        let old_world_time = WORLD_TIME.load(Ordering::Relaxed);
        let old_percentage = command_gamerules()
            .lock()
            .unwrap()
            .get("playersSleepingPercentage")
            .copied();
        let (client, _peer) = effect_tests::client_pair();
        let client = Arc::new(client);
        clients().lock().unwrap().push(client.clone());

        WORLD_TIME.store(13000, Ordering::Relaxed);
        enter_bed(&client, 0, 64, 0);
        assert!(client.sleeping.lock().unwrap().is_some());
        // Entering a bed does not skip the night immediately.
        assert_eq!(WORLD_TIME.load(Ordering::Relaxed), 13000);
        for _ in 0..99 {
            sleep_tick();
        }
        assert_eq!(WORLD_TIME.load(Ordering::Relaxed), 13000);
        assert!(client.sleeping.lock().unwrap().is_some());
        sleep_tick();
        assert_eq!(WORLD_TIME.load(Ordering::Relaxed), 24000);
        assert!(client.sleeping.lock().unwrap().is_none());

        // playersSleepingPercentage > 100 can never be met.
        WORLD_TIME.store(13000, Ordering::Relaxed);
        command_gamerules()
            .lock()
            .unwrap()
            .insert("playersSleepingPercentage", 110);
        enter_bed(&client, 0, 64, 0);
        for _ in 0..150 {
            sleep_tick();
        }
        assert_eq!(WORLD_TIME.load(Ordering::Relaxed), 13000);
        wake_if_sleeping(&client);

        // Waking before the 100th tick keeps the night going.
        match old_percentage {
            Some(v) => {
                command_gamerules()
                    .lock()
                    .unwrap()
                    .insert("playersSleepingPercentage", v);
            }
            None => {
                command_gamerules()
                    .lock()
                    .unwrap()
                    .remove("playersSleepingPercentage");
            }
        }
        enter_bed(&client, 0, 64, 0);
        for _ in 0..50 {
            sleep_tick();
        }
        assert!(wake_if_sleeping(&client));
        for _ in 0..100 {
            sleep_tick();
        }
        assert_eq!(WORLD_TIME.load(Ordering::Relaxed), 13000);

        *clients().lock().unwrap() = old_clients;
        WORLD_TIME.store(old_world_time, Ordering::Relaxed);
    }

    #[test]
    fn protocol_sends_view_and_sim_distance_and_validates_container_state_id() {
        let _serial = SLEEP_TEST_LOCK.lock().unwrap();
        // Vanilla 1.21.1 default: 256; `0`/negative disables; positive is the
        // negotiated threshold.
        assert_eq!(connection_limits::compression_threshold_env(|_| None), 256);
        for (raw, want) in [("-1", -1), ("0", -1), ("64", 64), ("junk", 256)] {
            let value = connection_limits::compression_threshold_env(|name| {
                (name == "MC_SPINE_COMPRESSION_THRESHOLD").then(|| raw.to_string())
            });
            assert_eq!(value, want, "threshold for {raw:?}");
        }

        let (client, mut peer) = effect_tests::client_pair();
        client.enter_play_wire(None, Default::default());
        send_view_distance(&mut client.sock.lock().unwrap(), SERVER_VIEW_DISTANCE);
        assert_eq!(
            read_packet(&mut peer, deadline()),
            Some((0x55, vec![SERVER_VIEW_DISTANCE as u8]))
        );
        assert_eq!(
            read_packet(&mut peer, deadline()),
            Some((0x62, vec![SERVER_SIMULATION_DISTANCE as u8]))
        );

        // A matching stateId is accepted; a stale one triggers a full 0x13
        // resync with a freshly incremented counter.
        assert!(!container_state_stale(&client, 0, 0));
        assert!(container_state_stale(&client, 0, 7));
        resend_container_content(&client, 0);
        assert_eq!(client.inv_state_id.load(Ordering::Relaxed), 1);
        let (id, body) = read_packet(&mut peer, deadline()).expect("resync packet");
        assert_eq!(id, 0x13);
        assert_eq!(body[0], 0, "window id");
        let mut sl = &body[1..];
        assert_eq!(read_varint(&mut sl), Some(1), "incremented stateId");
        assert_eq!(read_varint(&mut sl), Some(46), "slot count");
        assert!(!container_state_stale(&client, 0, 1));
    }

    static SLEEP_TEST_LOCK: Mutex<()> = Mutex::new(());
}

#[cfg(test)]
mod w3_tests {
    use super::*;
    use std::net::TcpListener;

    fn w3_deadline() -> Instant {
        Instant::now() + Duration::from_secs(15)
    }

    /// Handshake + Login Start on a fresh loopback connection, then the raw
    /// socket; the server side is a spawned `handle` on the accepted half.
    fn w3_login_session(name: &str, uuid: [u8; 16]) -> (TcpStream, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (srv, _) = listener.accept().unwrap();
            handle(srv);
        });
        let mut cli = TcpStream::connect(addr).unwrap();
        cli.set_nodelay(true).ok();
        cli.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        let mut hs = Vec::new();
        write_varint(&mut hs, 767);
        write_str(&mut hs, "127.0.0.1");
        hs.extend_from_slice(&25565u16.to_be_bytes());
        write_varint(&mut hs, 2);
        send_packet(&mut cli, 0x00, hs).unwrap();
        let mut login = Vec::new();
        write_str(&mut login, name);
        login.extend_from_slice(&uuid);
        send_packet(&mut cli, 0x00, login).unwrap();
        (cli, server)
    }

    /// `w3_login_session` continued through configuration into the play loop.
    fn w3_connect(name: &str, uuid: [u8; 16]) -> (TcpStream, std::thread::JoinHandle<()>, Arc<Client>) {
        let (mut cli, server) = w3_login_session(name, uuid);
        let deadline = w3_deadline();
        loop {
            let (pid, _) = read_packet(&mut cli, deadline).expect("login success");
            if pid == 0x02 {
                break;
            }
        }
        send_packet(&mut cli, 0x03, vec![]).unwrap(); // Login Acknowledged
        let mut ci = Vec::new();
        write_str(&mut ci, "en_us");
        ci.push(2);
        write_varint(&mut ci, 0);
        ci.push(1);
        ci.push(0x7f);
        write_varint(&mut ci, 1);
        ci.push(0);
        ci.push(1);
        send_packet(&mut cli, 0x00, ci).unwrap();
        loop {
            let (pid, _) = read_packet(&mut cli, deadline).expect("config packets");
            if pid == 0x0e {
                let mut kp = Vec::new();
                write_varint(&mut kp, 0);
                send_packet(&mut cli, 0x07, kp).unwrap();
            } else if pid == 0x03 {
                send_packet(&mut cli, 0x03, vec![]).unwrap();
                break;
            }
        }
        loop {
            let (pid, body) = read_packet(&mut cli, deadline).expect("play position");
            if pid == 0x40 {
                send_packet(&mut cli, 0x00, body[33..].to_vec()).unwrap();
                break;
            }
        }
        let reg_deadline = Instant::now() + Duration::from_secs(2);
        let me = loop {
            if let Some(found) = clients()
                .lock()
                .unwrap()
                .iter()
                .find(|c| c.name == name)
                .cloned()
            {
                break found;
            }
            assert!(Instant::now() < reg_deadline, "registered play client");
            std::thread::sleep(Duration::from_millis(5));
        };
        (cli, server, me)
    }

    /// Read until `wanted` arrives or the deadline passes; returns the payload.
    fn w3_read_until(cli: &mut TcpStream, wanted: i64) -> Option<Vec<u8>> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while let Some((pid, body)) = read_packet(cli, deadline) {
            if pid == wanted {
                return Some(body);
            }
        }
        None
    }

    /// A VarInt-counted `long[]` bitset off the wire, like `readProtocol.i` in
    /// vanilla's `FriendlyByteBuf`.
    fn w3_read_bitset(b: &mut &[u8]) -> Vec<u64> {
        let count = usize::try_from(read_protocol_i32(b).expect("bitset word count"))
            .expect("bitset word count");
        let mut words = Vec::with_capacity(count);
        for _ in 0..count {
            let (head, tail) = b.split_at(8);
            words.push(u64::from_be_bytes(head.try_into().unwrap()));
            *b = tail;
        }
        words
    }

    fn w3_test_admission(
        bans: BanRegistry,
        whitelist: WhitelistRegistry,
        max_players: usize,
    ) -> LiveAdmission {
        LiveAdmission {
            ops: OpsRegistry::default(),
            bans,
            whitelist,
            max_players,
        }
    }

    #[test]
    fn w3_login_admission_rejects_banned_and_full_server() {
        let banned = BanRegistry {
            players: vec![BanEntry {
                uuid: "00000000-0000-0000-0000-000000000001".into(),
                name: "Banned_Player".into(),
                created: "2024-01-01T00:00:00Z".into(),
                source: "test".into(),
                expires: "forever".into(),
                reason: "test ban".into(),
            }],
            ips: Vec::new(),
        };
        let cases: Vec<(&str, LiveAdmission, &str)> = vec![
            (
                "Banned_Player",
                w3_test_admission(banned.clone(), WhitelistRegistry::default(), 20),
                "You are banned from this server.\nReason: test ban",
            ),
            (
                "NotOnTheList",
                w3_test_admission(
                    BanRegistry::default(),
                    WhitelistRegistry {
                        enabled: true,
                        entries: Vec::new(),
                    },
                    20,
                ),
                "You are not white-listed on this server!",
            ),
            (
                "TooLate",
                w3_test_admission(BanRegistry::default(), WhitelistRegistry::default(), 0),
                "The server is full!",
            ),
        ];
        for (name, admission, reason) in cases {
            *live_admission().lock().unwrap() = Some(admission);
            let (mut cli, server) = w3_login_session(name, [0xabu8; 16]);
            let (pid, body) = read_packet(&mut cli, w3_deadline()).expect("disconnect packet");
            assert_eq!(pid, 0x00, "login must be refused with 0x00 for {name}");
            let mut sl = &body[..];
            let json = {
                let n = usize::try_from(read_varint(&mut sl).unwrap()).unwrap();
                std::str::from_utf8(&sl[..n]).unwrap().to_owned()
            };
            assert!(
                reason.lines().all(|l| json.contains(l)),
                "disconnect for {name} must carry the vanilla text, got {json}"
            );
            drop(cli);
            let _ = server.join();
        }
        *live_admission().lock().unwrap() = None;
    }

    #[test]
    fn w3_vehicle_move_0x1e_and_dismount_0x26_and_stats_0x09_live() {
        let (mut cli, server, me) = w3_connect("W3Rider", [0x77u8; 16]);
        let (px, py, pz, ..) = *me.pos.lock().unwrap();
        let boat_eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
        let boat = Arc::new(Mob {
            dim: DimId::Overworld,
            eid: boat_eid,
            uuid: [0x55u8; 16],
            etype: name_to_etype("boat").unwrap(),
            pos: Mutex::new((px + 1.0, py, pz, 0.0, 0.0)),
            hp: Mutex::new(1.0),
            last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
            hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
            last_hurt_tick: Mutex::new(None),
            fuse: Mutex::new(0),
            fire_ticks: Mutex::new(0),
            love: Mutex::new(0),
            air: Mutex::new(300),
            vel: Mutex::new([0.0; 3]),
        });
        mobs().lock().unwrap().push(boat.clone());
        me.seen.lock().unwrap().insert(boat_eid);

        // 0x16 interact -> mounted, 0x5f broadcast reaches the tracker.
        let mut interact = Vec::new();
        write_varint(&mut interact, i64::from(boat_eid));
        write_varint(&mut interact, 0);
        send_packet(&mut cli, 0x16, interact).unwrap();
        let body = w3_read_until(&mut cli, 0x5f).expect("set passengers on mount");
        let mut rest = &body[..];
        assert_eq!(
            decode_set_passengers(&mut rest),
            Some((boat_eid, vec![me.eid])),
            "mount must broadcast the rider on the vehicle"
        );
        assert_eq!(vehicle_of(me.eid), Some(boat_eid));

        // 0x1e MoveVehicle: a small on-land step is accepted, moves the boat,
        // re-seats the rider and echoes a 0x70 teleport to the trackers.
        let mut mv = Vec::new();
        mv.extend_from_slice(&(px + 1.05f64).to_be_bytes());
        mv.extend_from_slice(&(py).to_be_bytes());
        mv.extend_from_slice(&(pz).to_be_bytes());
        mv.extend_from_slice(&0f32.to_be_bytes());
        mv.extend_from_slice(&0f32.to_be_bytes());
        send_packet(&mut cli, 0x1e, mv).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while boat.pos.lock().unwrap().0 <= px + 1.0 + 1.0e-9 {
            assert!(Instant::now() < deadline, "boat never moved");
            std::thread::sleep(Duration::from_millis(2));
        }
        let seat = *me.pos.lock().unwrap();
        assert!(
            (seat.0 - (px + 1.05)).abs() < 1.0e-6,
            "rider must be re-seated onto the moved vehicle, got {seat:?}"
        );

        // 0x26 PlayerInput with the unmount bit dismounts and re-broadcasts
        // the now-empty passenger list; the rider is re-positioned by teleport.
        let mut input = Vec::new();
        input.extend_from_slice(&0f32.to_be_bytes());
        input.extend_from_slice(&0f32.to_be_bytes());
        input.push(0x02);
        send_packet(&mut cli, 0x26, input).unwrap();
        let body = w3_read_until(&mut cli, 0x5f).expect("set passengers on dismount");
        let mut rest = &body[..];
        assert_eq!(
            decode_set_passengers(&mut rest),
            Some((boat_eid, Vec::new())),
            "dismount must broadcast an empty passenger list"
        );
        assert_eq!(vehicle_of(me.eid), None);

        // 0x09 action 1 (Request Stats) -> 0x04 AwardStats with the live book.
        stat_increment(&me, StatCategory::Mined, 1, 7);
        let mut req = Vec::new();
        write_varint(&mut req, 1);
        send_packet(&mut cli, 0x09, req).unwrap();
        let body = w3_read_until(&mut cli, 0x04).expect("award stats reply");
        let mut rest = &body[..];
        let stats = decode_award_stats(&mut rest).expect("valid award stats");
        assert_eq!(stats.get(StatCategory::Mined, 1), 7);

        mobs().lock().unwrap().retain(|m| m.eid != boat_eid);
        drop(cli);
        let _ = server.join();
    }

    #[test]
    fn w3_tall_custom_dim_over_64_sections_encodes_multiword_light_mask() {
        // 1024-block-tall custom dimension: 64 block sections + the 2
        // light-only guard sections = 66 light sections -> a 2-word bitset.
        worldgen::reset_dimension_registry();
        let sources = vec![worldgen::DatapackDimensionSource::Entries(vec![
            (
                "data/w3/dimension_type/tall_type.json".to_string(),
                r##"{
                    "ultrawarm": false, "natural": false, "coordinate_scale": 1.0,
                    "has_skylight": false, "has_ceiling": false, "ambient_light": 0.0,
                    "piglin_safe": false, "bed_works": false,
                    "respawn_anchor_works": false, "has_raids": false,
                    "logical_height": 1024, "min_y": 0, "height": 1024,
                    "infiniburn": "#minecraft:infiniburn_overworld",
                    "effects": "minecraft:overworld",
                    "monster_spawn_light_level": 0,
                    "monster_spawn_block_light_limit": 0
                }"##
                .to_string(),
            ),
            (
                "data/w3/dimension/tall.json".to_string(),
                r#"{"type": "w3:tall_type", "generator": {"type": "minecraft:debug"}}"#
                    .to_string(),
            ),
        ])];
        assert_eq!(
            worldgen::register_datapack_dimensions(&sources, |_| Some(1)).unwrap(),
            1
        );
        let tall = DimId::parse_dimension_id("w3:tall").expect("custom dimension resolves");
        assert_eq!(tall.light_sections(), 66);
        assert_eq!(tall.light_mask_longs(), vec![u64::MAX, 0b11]);

        // The chunk-join writer: the sky half is empty for a no-skylight dim;
        // the block half carries the full 2-word masks rather than a single
        // saturated u64.
        let gen = GenSources::new();
        let mut p = Vec::new();
        write_light_arrays(&mut p, 0, 0, tall, &gen);
        let mut light = &p[..];
        let sky_mask = w3_read_bitset(&mut light);
        let block_mask = w3_read_bitset(&mut light);
        let sky_empty = w3_read_bitset(&mut light);
        let block_empty = w3_read_bitset(&mut light);
        assert!(sky_mask.is_empty() && sky_empty.is_empty());
        // Words are trimmed like `BitSet.toLongArray`, so cover every section
        // index across mask+empty rather than comparing word vectors blindly.
        let cover: Vec<u64> = (0..tall.light_mask_longs().len())
            .map(|i| {
                block_mask.get(i).copied().unwrap_or(0)
                    | block_empty.get(i).copied().unwrap_or(0)
            })
            .collect();
        assert_eq!(
            cover,
            tall.light_mask_longs(),
            "every light section must sit in mask or empty for all 66 sections"
        );

        // The update writer over a synthetic 130-section payload: the lit
        // section lands in bit 65 (word 2), proving multi-word placement.
        let mut sections = vec![[0u8; 2048]; 130];
        sections[129][0] = 1;
        let wire = block_light_update((0, 0), &sections);
        let mut light = &wire[2..]; // skip the two cx/cz varints
        let sky_mask = w3_read_bitset(&mut light);
        let block_mask = w3_read_bitset(&mut light);
        let sky_empty = w3_read_bitset(&mut light);
        let block_empty = w3_read_bitset(&mut light);
        assert!(sky_mask.is_empty() && sky_empty.is_empty());
        assert_eq!(block_mask, vec![0, 0, 0b10]);
        assert_eq!(block_empty, vec![u64::MAX, u64::MAX, 0b01]);
        assert_eq!(read_protocol_i32(&mut light), Some(0), "no sky arrays");
        assert_eq!(
            read_protocol_i32(&mut light),
            Some(1),
            "exactly one block-light array may follow"
        );

        // The same encode through the lane update path produces the same
        // 2-word empty coverage for the 66-section dim.
        let packet = update_light_packet_lane(tall, 0, 0, &gen, None);
        let mut light = &packet[2..];
        let _sky = w3_read_bitset(&mut light);
        let _block = w3_read_bitset(&mut light);
        let _sky_empty = w3_read_bitset(&mut light);
        let block_empty = w3_read_bitset(&mut light);
        assert_eq!(block_empty, tall.light_mask_longs());

        worldgen::reset_dimension_registry();
    }

    #[test]
    fn bughunt_net_look_packet_0x1c_rejects_non_finite_angles() {
        let (mut cli, server, me) = w3_connect("BugHuntLook", [0x66u8; 16]);
        *me.pos.lock().unwrap() = (0.5, 71.0, 0.5, 10.0, 20.0, true);
        let look = |yaw: f32, pitch: f32| {
            let mut b = Vec::new();
            b.extend_from_slice(&yaw.to_be_bytes());
            b.extend_from_slice(&pitch.to_be_bytes());
            b.push(1);
            b
        };
        // NaN and both infinities in either angle must not reach `pos`.
        for (yaw, pitch) in [
            (f32::NAN, 0.0),
            (0.0, f32::NAN),
            (f32::INFINITY, 0.0),
            (0.0, f32::NEG_INFINITY),
        ] {
            send_packet(&mut cli, 0x1c, look(yaw, pitch)).unwrap();
            std::thread::sleep(Duration::from_millis(80));
            let p = *me.pos.lock().unwrap();
            assert_eq!((p.3, p.4), (10.0, 20.0), "non-finite look {yaw}/{pitch} applied");
        }
        // A finite look still lands, proving the packet path executed.
        send_packet(&mut cli, 0x1c, look(90.0, -15.0)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let p = *me.pos.lock().unwrap();
            if p.3 == 90.0 && p.4 == -15.0 {
                break;
            }
            assert!(Instant::now() < deadline, "finite look never applied");
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(cli);
        let _ = server.join();
    }

    #[test]
    fn bughunt_net_dim_wire_ids_skip_custom_dimension_slots() {
        let _world_guard = world_guard();
        let _gen_guard = worldgen::lock_gen_for_tests();
        worldgen::reset_dimension_registry();
        let sources = vec![worldgen::DatapackDimensionSource::Entries(vec![
            (
                "data/bhn/dimension_type/bhn_type.json".to_string(),
                r##"{
                    "ultrawarm": false, "natural": false, "coordinate_scale": 1.0,
                    "has_skylight": false, "has_ceiling": false, "ambient_light": 0.0,
                    "piglin_safe": false, "bed_works": false,
                    "respawn_anchor_works": false, "has_raids": false,
                    "logical_height": 256, "min_y": 0, "height": 256,
                    "infiniburn": "#minecraft:infiniburn_overworld",
                    "effects": "minecraft:overworld",
                    "monster_spawn_light_level": 0,
                    "monster_spawn_block_light_limit": 0
                }"##
                .to_string(),
            ),
            (
                "data/bhn/dimension/net_dim.json".to_string(),
                r#"{"type": "bhn:bhn_type", "generator": {"type": "minecraft:debug"}}"#
                    .to_string(),
            ),
        ])];
        assert_eq!(
            worldgen::register_datapack_dimensions(&sources, |_| Some(1)).unwrap(),
            1
        );
        let custom = DimId::parse_dimension_id("bhn:net_dim").expect("custom dimension resolves");
        assert!(custom.index() >= 3, "custom slot follows the vanilla three");
        // A registry whose entry names include the custom dimension id used to
        // index `ids[dim.index()]` past the end of the three-slot table.
        let entries: Vec<(&str, &[u8])> = vec![
            ("minecraft:overworld", &[]),
            ("minecraft:the_nether", &[]),
            ("minecraft:the_end", &[]),
            ("bhn:net_dim", &[]),
        ];
        assert_eq!(
            dim_wire_ids_from_entries(entries),
            Some([0, 1, 2]),
            "vanilla wire ids must still resolve with a custom slot present"
        );
        worldgen::reset_dimension_registry();
    }
}

#[cfg(test)]
mod fix_tame_rejoin_tests {
    use super::*;
    use std::net::TcpListener;

    fn fx_deadline() -> Instant {
        Instant::now() + Duration::from_secs(15)
    }

    /// Handshake + Login Start on a fresh loopback connection, then the raw
    /// socket; the server side is a spawned `handle` on the accepted half.
    fn fx_login_session(name: &str, uuid: [u8; 16]) -> (TcpStream, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (srv, _) = listener.accept().unwrap();
            handle(srv);
        });
        let mut cli = TcpStream::connect(addr).unwrap();
        cli.set_nodelay(true).ok();
        cli.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        let mut hs = Vec::new();
        write_varint(&mut hs, 767);
        write_str(&mut hs, "127.0.0.1");
        hs.extend_from_slice(&25565u16.to_be_bytes());
        write_varint(&mut hs, 2);
        send_packet(&mut cli, 0x00, hs).unwrap();
        let mut login = Vec::new();
        write_str(&mut login, name);
        login.extend_from_slice(&uuid);
        send_packet(&mut cli, 0x00, login).unwrap();
        (cli, server)
    }

    /// `fx_login_session` through configuration into the play loop, also
    /// capturing the 0x54 SetChunkCacheCenter body sent right after Join Game.
    fn fx_connect(
        name: &str,
        uuid: [u8; 16],
    ) -> (TcpStream, std::thread::JoinHandle<()>, Arc<Client>, Vec<u8>) {
        let (mut cli, server) = fx_login_session(name, uuid);
        let deadline = fx_deadline();
        loop {
            let (pid, _) = read_packet(&mut cli, deadline).expect("login success");
            if pid == 0x02 {
                break;
            }
        }
        send_packet(&mut cli, 0x03, vec![]).unwrap();
        let mut ci = Vec::new();
        write_str(&mut ci, "en_us");
        ci.push(2);
        write_varint(&mut ci, 0);
        ci.push(1);
        ci.push(0x7f);
        write_varint(&mut ci, 1);
        ci.push(0);
        ci.push(1);
        send_packet(&mut cli, 0x00, ci).unwrap();
        loop {
            let (pid, _) = read_packet(&mut cli, deadline).expect("config packets");
            if pid == 0x0e {
                let mut kp = Vec::new();
                write_varint(&mut kp, 0);
                send_packet(&mut cli, 0x07, kp).unwrap();
            } else if pid == 0x03 {
                send_packet(&mut cli, 0x03, vec![]).unwrap();
                break;
            }
        }
        let mut center = Vec::new();
        loop {
            let (pid, body) = read_packet(&mut cli, deadline).expect("play position");
            if pid == 0x54 {
                center = body.clone();
            }
            if pid == 0x40 {
                send_packet(&mut cli, 0x00, body[33..].to_vec()).unwrap();
                break;
            }
        }
        let reg_deadline = Instant::now() + Duration::from_secs(2);
        let me = loop {
            if let Some(found) = clients()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .find(|c| c.name == name)
                .cloned()
            {
                break found;
            }
            assert!(Instant::now() < reg_deadline, "registered play client");
            std::thread::sleep(Duration::from_millis(5));
        };
        (cli, server, me, center)
    }

    /// The offline-mode UUID `login_identity` derives from the player name —
    /// the key `saved_players` is read under on rejoin.
    fn fx_offline_uuid(name: &str) -> [u8; 16] {
        let mut uuid = generation_math::md5(format!("OfflinePlayer:{name}").as_bytes());
        uuid[6] = (uuid[6] & 0x0f) | 0x30;
        uuid[8] = (uuid[8] & 0x3f) | 0x80;
        uuid
    }

    fn fx_save(uuid: [u8; 16], position: DimPosRot) {
        saved_players()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                uuid,
                PlayerSave {
                    position,
                    inventory: HashMap::new(),
                    ender_chest: HashMap::new(),
                    held: 0,
                    health: 20.0,
                    absorption: 0.0,
                    food: 20,
                    saturation: 5.0,
                    exhaustion: 0.0,
                    xp_level: 0,
                    xp_progress: 0.0,
                    xp_total: 0,
                    gamemode: 0,
                    effects: Vec::new(),
                    respawn: None,
                },
            );
    }

    fn fx_spawn_mob(etype: i64, pos: (f64, f64, f64)) -> Arc<Mob> {
        let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
        let mob = Arc::new(Mob {
            dim: DimId::Overworld,
            eid,
            uuid: [0x77u8; 16],
            etype,
            pos: Mutex::new((pos.0, pos.1, pos.2, 0.0, 0.0)),
            hp: Mutex::new(8.0),
            last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
            hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
            last_hurt_tick: Mutex::new(None),
            fuse: Mutex::new(0),
            fire_ticks: Mutex::new(0),
            love: Mutex::new(0),
            air: Mutex::new(300),
            vel: Mutex::new([0.0; 3]),
        });
        mobs().lock().unwrap_or_else(|e| e.into_inner()).push(mob.clone());
        live_mob_states()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(eid, LiveMobState::new(etype));
        mob
    }

    fn fx_pet(eid: i32) -> Option<(Option<[u8; 16]>, bool, f32, u8)> {
        let states = live_mob_states().lock().unwrap_or_else(|e| e.into_inner());
        match &states.get(&eid)?.behavior {
            Some(MobBehaviorState::Wolf(pet) | MobBehaviorState::Cat(pet)) => {
                Some((pet.owner, pet.sitting, pet.hp, pet.collar_color))
            }
            _ => None,
        }
    }

    #[test]
    fn fix_tame_rejoin_chunk_cache_center_uses_saved_position() {
        let uuid = fx_offline_uuid("FarRejoin");
        fx_save(uuid, (DimId::Overworld, 1000.5, 70.0, -640.2, 0.0, 0.0));
        let (cli, server, _me, center) = fx_connect("FarRejoin", uuid);
        assert!(!center.is_empty(), "0x54 SetChunkCacheCenter must arrive");
        let mut sl = &center[..];
        // Chunk coords are i32 varints; a negative reads back zero-extended.
        let cx = read_varint(&mut sl).unwrap() as i32;
        let cz = read_varint(&mut sl).unwrap() as i32;
        assert_eq!(
            (cx, cz),
            (62, -41),
            "rejoin at (1000.5, -640.2) must center chunk (62, -41), not (0, 0)"
        );
        drop(cli);
        let _ = server.join();
        saved_players()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&uuid);
    }

    #[test]
    fn fix_tame_rejoin_disconnect_clears_spectator_camera() {
        let (cli, server, me, _center) = fx_connect("SpecCam", [0x66u8; 16]);
        {
            let mut cams = spectator_cameras().lock().unwrap_or_else(|e| e.into_inner());
            cams.insert(me.eid, 31337);
            cams.insert(42424, me.eid);
        }
        drop(cli);
        let _ = server.join();
        {
            let cams = spectator_cameras().lock().unwrap_or_else(|e| e.into_inner());
            assert!(
                !cams.contains_key(&me.eid),
                "a leaver's own camera entry must drop on disconnect"
            );
            assert!(
                !cams.values().any(|target| *target == me.eid),
                "no spectator camera may keep targeting the leaver"
            );
        }
        spectator_cameras()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&42424);
    }

    #[test]
    fn fix_tame_rejoin_wolf_tame_sit_dye_stranger_rejected() {
        let (mut owner, _os) = effect_tests::client_pair();
        owner.uuid = [0x11u8; 16];
        let (mut stranger, _ss) = effect_tests::client_pair();
        stranger.uuid = [0x22u8; 16];
        let wolf = fx_spawn_mob(122, (10.0, 70.0, 10.0));
        for c in [&owner, &stranger] {
            c.seen
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(wolf.eid);
        }
        // A fed bone is eaten every try, tame or not (survival consumption).
        owner
            .inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(36, (961, 8, Vec::new()));
        live_mob_interact(&owner, wolf.eid, 36);
        assert_eq!(
            owner
                .inv
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&36)
                .map(|stack| stack.1),
            Some(7),
            "a bone fed to an untamed wolf is consumed"
        );
        let mut tamed = false;
        for _ in 0..60 {
            // A tame leaves the wolf sitting; check before clicking so an
            // owner-click does not toggle it right back off.
            if fx_pet(wolf.eid).and_then(|p| p.0).is_some() {
                tamed = true;
                break;
            }
            if owner
                .inv
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&36)
                .is_none()
            {
                owner
                    .inv
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(36, (961, 8, Vec::new()));
            }
            live_mob_interact(&owner, wolf.eid, 36);
        }
        tamed |= fx_pet(wolf.eid).and_then(|p| p.0).is_some();
        assert!(tamed, "bones tame the wolf within the deterministic rolls");
        let (pet_owner, sitting, hp, collar) = fx_pet(wolf.eid).expect("wolf pet state");
        assert_eq!(pet_owner, Some(owner.uuid));
        assert!(sitting, "a freshly tamed wolf sits");
        assert_eq!(hp, 40.0, "a tamed wolf's max health is 40");
        assert_eq!(*wolf.hp.lock().unwrap_or_else(|e| e.into_inner()), 40.0);
        assert_eq!(collar, 14);

        // Strangers cannot sit or dye someone else's pet.
        stranger
            .inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(36, (955, 1, Vec::new()));
        assert!(!pet_collar_or_parrot_interact(&stranger, wolf.eid, 36));
        assert!(!live_mob_interact(&stranger, wolf.eid, 36));
        assert_eq!(fx_pet(wolf.eid).map(|p| p.3), Some(14));
        assert_eq!(fx_pet(wolf.eid).map(|p| p.1), Some(true));

        // Owner empty hand toggles sitting.
        owner
            .inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&36);
        assert!(live_mob_interact(&owner, wolf.eid, 36));
        assert_eq!(fx_pet(wolf.eid).map(|p| p.1), Some(false));
        assert!(live_mob_interact(&owner, wolf.eid, 36));
        assert_eq!(fx_pet(wolf.eid).map(|p| p.1), Some(true));

        // Owner dye colors the collar and consumes one dye in survival.
        owner
            .inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(36, (955, 1, Vec::new()));
        assert!(pet_collar_or_parrot_interact(&owner, wolf.eid, 36));
        assert_eq!(fx_pet(wolf.eid).map(|p| p.3), Some(11));
        assert!(
            owner
                .inv
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&36)
                .is_none(),
            "the dye used on the collar is consumed"
        );

        mobs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|m| m.eid != wolf.eid);
        live_mob_states()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&wolf.eid);
    }

    #[test]
    fn fix_tame_rejoin_parrot_seed_tame_and_owner_sit() {
        let (mut owner, _os) = effect_tests::client_pair();
        owner.uuid = [0x33u8; 16];
        let (mut stranger, _ss) = effect_tests::client_pair();
        stranger.uuid = [0x44u8; 16];
        let parrot = fx_spawn_mob(75, (10.0, 70.0, 10.0));
        for c in [&owner, &stranger] {
            c.seen
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(parrot.eid);
        }
        owner
            .inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(36, (853, 8, Vec::new()));
        assert!(pet_collar_or_parrot_interact(&owner, parrot.eid, 36));
        assert_eq!(
            owner
                .inv
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&36)
                .map(|stack| stack.1),
            Some(7),
            "a seed fed to an untamed parrot is consumed"
        );
        let mut tamed = false;
        for _ in 0..500 {
            // Same guard as the wolf loop: a tame sits the parrot, so check
            // before clicking or the next owner-click toggles it off again.
            if parrot_pets()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&parrot.eid)
                .is_some_and(|pet| pet.owner.is_some())
            {
                tamed = true;
                break;
            }
            if owner
                .inv
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&36)
                .is_none()
            {
                owner
                    .inv
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(36, (853, 8, Vec::new()));
            }
            pet_collar_or_parrot_interact(&owner, parrot.eid, 36);
        }
        assert!(tamed, "seeds tame the parrot within the 1-in-10 rolls");
        assert!(parrot_pets()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&parrot.eid)
            .is_some_and(|pet| pet.owner == Some(owner.uuid) && pet.sitting));

        // Strangers cannot toggle the parrot; the owner can.
        assert!(!pet_collar_or_parrot_interact(&stranger, parrot.eid, 36));
        assert!(pet_collar_or_parrot_interact(&owner, parrot.eid, 36));
        assert!(parrot_pets()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&parrot.eid)
            .is_some_and(|pet| !pet.sitting));

        mobs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|m| m.eid != parrot.eid);
        live_mob_states()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&parrot.eid);
        parrot_pets()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&parrot.eid);
    }

    #[test]
    fn fix_tame_rejoin_pet_follow_teleports_only_to_safe_floor() {
        let owner = (0.0, 64.0, 0.0);
        let floor = |_: DimId, _x: i32, y: i32, _z: i32| y == 63;
        // Standing and 13 blocks out: teleport to the first safe floor scan hit.
        assert_eq!(
            pet_follow_teleport_step(DimId::Overworld, owner, (13.0, 64.0, 0.0), false, floor),
            Some((-2.5, 64.0, -2.5))
        );
        // Sitting pets never teleport.
        assert_eq!(
            pet_follow_teleport_step(DimId::Overworld, owner, (13.0, 64.0, 0.0), true, floor),
            None
        );
        // Within the 12-block leash there is nothing to do.
        assert_eq!(
            pet_follow_teleport_step(DimId::Overworld, owner, (8.0, 64.0, 8.0), false, floor),
            None
        );
        // Exactly 12 blocks is still in range (144.0 is not > 144.0).
        assert_eq!(
            pet_follow_teleport_step(DimId::Overworld, owner, (12.0, 64.0, 0.0), false, floor),
            None
        );
        // No safe floor anywhere near the owner: no teleport.
        assert_eq!(
            pet_follow_teleport_step(
                DimId::Overworld,
                owner,
                (13.0, 64.0, 0.0),
                false,
                |_: DimId, _x: i32, _y: i32, _z: i32| false
            ),
            None
        );
    }
}

#[cfg(test)]
mod w4_tests {
    use super::*;
    use std::net::TcpListener;

    fn w4_deadline() -> Instant {
        Instant::now() + Duration::from_secs(15)
    }

    /// Restores env vars and shared wiring state on drop — including under a
    /// panic, so a failed assert cannot poison the tests that run after it.
    struct W4Env {
        vars: Vec<(&'static str, Option<String>)>,
    }
    impl W4Env {
        fn new() -> Self {
            W4Env { vars: Vec::new() }
        }
        fn set(mut self, name: &'static str, value: &str) -> Self {
            self.vars.push((name, std::env::var(name).ok()));
            std::env::set_var(name, value);
            self
        }
    }
    impl std::ops::Drop for W4Env {
        fn drop(&mut self) {
            for (name, old) in self.vars.drain(..) {
                match old {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
            *live_admission().lock().unwrap() = None;
            set_online_auth_provider(None);
        }
    }

    /// The client half of a loopback session: a `ConnStream` so the same code
    /// drives both the plain and the AES-128-CFB8 wire forms.
    fn w4_client() -> (ConnStream, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (srv, _) = listener.accept().unwrap();
            handle(srv);
        });
        let cli = TcpStream::connect(addr).unwrap();
        cli.set_nodelay(true).ok();
        cli.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        (ConnStream::from(cli), server)
    }

    fn w4_send(cli: &mut ConnStream, id: i64, payload: &[u8]) {
        let mut body = Vec::new();
        write_varint(&mut body, id);
        body.extend_from_slice(payload);
        let mut frame = Vec::new();
        write_varint(&mut frame, body.len() as i64);
        frame.extend_from_slice(&body);
        cli.write_all(&frame).unwrap();
    }

    fn w4_read_exact(cli: &mut ConnStream, mut out: &mut [u8], deadline: Instant) -> Option<()> {
        while !out.is_empty() {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            cli.tcp().set_read_timeout(Some(remaining)).ok()?;
            let n = cli.read(out).ok()?;
            if n == 0 {
                return None;
            }
            out = &mut out[n..];
        }
        Some(())
    }

    /// Frame + packet-id decode on the test client, like `read_login_frame`
    /// without compression (the test build negotiates none by default).
    fn w4_read_packet(cli: &mut ConnStream, deadline: Instant) -> Option<(i64, Vec<u8>)> {
        let mut len = 0u64;
        let mut done = false;
        for i in 0..3 {
            let mut b = [0u8; 1];
            w4_read_exact(cli, &mut b, deadline)?;
            len |= ((b[0] & 0x7f) as u64) << (7 * i);
            if b[0] & 0x80 == 0 {
                done = true;
                break;
            }
        }
        if !done || len == 0 || len > 64 * 1024 {
            return None;
        }
        let mut body = vec![0u8; len as usize];
        w4_read_exact(cli, &mut body, deadline)?;
        let mut payload = body.as_slice();
        let id = read_protocol_i32(&mut payload)?;
        Some((i64::from(id), payload.to_vec()))
    }

    /// Handshake (next=2) + Login Start through the current transport.
    fn w4_login_start(cli: &mut ConnStream, name: &str, uuid: [u8; 16]) {
        let mut hs = Vec::new();
        write_varint(&mut hs, 767);
        write_str(&mut hs, "127.0.0.1");
        hs.extend_from_slice(&25565u16.to_be_bytes());
        write_varint(&mut hs, 2);
        w4_send(cli, 0x00, &hs);
        let mut login = Vec::new();
        write_str(&mut login, name);
        login.extend_from_slice(&uuid);
        w4_send(cli, 0x00, &login);
    }

    /// The first packet carrying `wanted`'s id, or the decode error that came
    /// instead; drains anything else the server sends first.
    fn w4_read_until(cli: &mut ConnStream, wanted: i64, deadline: Instant) -> Option<Vec<u8>> {
        while let Some((pid, body)) = w4_read_packet(cli, deadline) {
            if pid == wanted {
                return Some(body);
            }
        }
        None
    }

    /// A signed `velocity:player_info` body: the forwarding fields followed by
    /// the 32-byte HMAC-SHA256 prefix `verify_velocity_forwarding` checks.
    fn w4_velocity_payload(
        secret: &[u8],
        ip: &str,
        uuid: [u8; 16],
        name: &str,
        properties: &[ForwardedProperty],
    ) -> Vec<u8> {
        let mut body = Vec::new();
        write_varint(&mut body, VELOCITY_FORWARDING_VERSION_MAX as i64);
        write_str(&mut body, ip);
        body.extend_from_slice(&uuid);
        write_str(&mut body, name);
        write_varint(&mut body, properties.len() as i64);
        for p in properties {
            write_str(&mut body, &p.name);
            write_str(&mut body, &p.value);
            match &p.signature {
                Some(sig) => {
                    body.push(1);
                    write_str(&mut body, sig);
                }
                None => body.push(0),
            }
        }
        let mut payload = hmac_sha256(secret, &body).to_vec();
        payload.extend_from_slice(&body);
        payload
    }

    /// After Login Start the forwarding gate asks for `velocity:player_info`;
    /// return its message id so the test can answer.
    fn w4_expect_plugin_request(cli: &mut ConnStream, deadline: Instant) -> i64 {
        let (pid, body) = w4_read_packet(cli, deadline).expect("plugin request");
        assert_eq!(pid, 0x04);
        let mut b = &body[..];
        let mid = read_varint(&mut b).unwrap();
        let channel_len = usize::try_from(read_varint(&mut b).unwrap()).unwrap();
        assert_eq!(&b[..channel_len], VELOCITY_FORWARDING_CHANNEL.as_bytes());
        assert_eq!(&b[channel_len..], &[VELOCITY_FORWARDING_VERSION_MAX as u8]);
        mid
    }

    fn w4_send_plugin_response(cli: &mut ConnStream, mid: i64, data: &[u8]) {
        let mut resp = Vec::new();
        write_varint(&mut resp, mid);
        resp.push(1); // successful
        resp.extend_from_slice(data);
        w4_send(cli, 0x02, &resp);
    }

    /// An offline online-mode provider: a fixed "public key", identity
    /// decryption, and a canned session profile; it records what the server
    /// asked it to verify so the test can check the vanilla server hash.
    struct W4Auth {
        key_der: Vec<u8>,
        token: Vec<u8>,
        profile: MojangJoinedProfile,
        decrypt_calls: Mutex<Vec<Vec<u8>>>,
        seen_hash: Mutex<Option<String>>,
        seen_ip: Mutex<Option<String>>,
    }
    impl OnlineAuth for W4Auth {
        fn public_key_der(&self) -> Vec<u8> {
            self.key_der.clone()
        }
        fn decrypt_login_field(&self, ciphertext: &[u8]) -> Option<Vec<u8>> {
            self.decrypt_calls
                .lock()
                .unwrap()
                .push(ciphertext.to_vec());
            Some(ciphertext.to_vec())
        }
        fn verify_token(&self) -> Vec<u8> {
            self.token.clone()
        }
        fn has_joined(
            &self,
            _username: &str,
            server_hash: &str,
            client_ip: Option<&str>,
        ) -> Result<MojangJoinedProfile, String> {
            *self.seen_hash.lock().unwrap() = Some(server_hash.to_string());
            *self.seen_ip.lock().unwrap() = client_ip.map(str::to_string);
            Ok(self.profile.clone())
        }
    }

    #[test]
    fn w4_status_ping_includes_player_sample_and_valid_favicon() {
        // The favicon cache is process-global; write the icon before the first
        // ping this binary ever answers so the OnceLock captures it.
        let icon = std::path::Path::new("server-icon.png");
        assert!(!icon.exists(), "test requires a clean working dir");
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&64u32.to_be_bytes());
        png.extend_from_slice(&64u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        std::fs::write(icon, &png).unwrap();

        // One registered client makes `players.sample` non-empty.
        let (mut viewer, _hold) = effect_tests::client_pair();
        viewer.name = "W4StatusViewer".to_string();
        viewer.uuid = [0xabu8; 16];
        let viewer = Arc::new(viewer);
        clients().lock().unwrap().push(viewer.clone());

        let (mut cli, server) = w4_client();
        let deadline = w4_deadline();
        let mut hs = Vec::new();
        write_varint(&mut hs, 767);
        write_str(&mut hs, "127.0.0.1");
        hs.extend_from_slice(&25565u16.to_be_bytes());
        write_varint(&mut hs, 1); // status
        w4_send(&mut cli, 0x00, &hs);
        w4_send(&mut cli, 0x00, &[]); // Status Request
        let body = w4_read_until(&mut cli, 0x00, deadline).expect("status response");
        let mut sl = &body[..];
        let n = usize::try_from(read_varint(&mut sl).unwrap()).unwrap();
        let text = std::str::from_utf8(&sl[..n]).unwrap().to_string();
        jparse(text.as_bytes()).expect("status response parses as json");
        assert!(text.contains("\"name\":\"1.21.1\",\"protocol\":767"), "{text}");
        assert!(text.contains("\"name\":\"W4StatusViewer\""), "{text}");
        assert!(
            text.contains("\"id\":\"abababab-abab-abab-abab-abababababab\""),
            "{text}"
        );
        assert!(
            text.contains("\"favicon\":\"data:image/png;base64,"),
            "{text}"
        );
        assert!(text.contains("\"text\":\"mc-spine\""), "{text}");

        // Ping keeps working after the rich response (payload echoes).
        w4_send(&mut cli, 0x01, &0xfeed_beefi64.to_be_bytes());
        let pong = w4_read_until(&mut cli, 0x01, deadline).expect("pong");
        assert_eq!(pong, 0xfeed_beefi64.to_be_bytes().to_vec());

        clients().lock().unwrap().retain(|c| c.eid != viewer.eid);
        drop(cli);
        let _ = server.join();
        let _ = std::fs::remove_file(icon);
    }

    #[test]
    fn w4_velocity_forwarding_accepts_signed_and_rejects_tampered_login() {
        let secret = "w4-shared-secret";
        let _env = W4Env::new()
            .set("MC_SPINE_VELOCITY_FORWARDING", "1")
            .set("MC_SPINE_VELOCITY_SECRET", secret);
        let deadline = w4_deadline();

        // A valid signed payload adopts the forwarded identity: the client lied
        // about its name/uuid in Login Start, and Login Success carries the
        // forwarded triple instead.
        let fwd_uuid = [0xabu8; 16];
        let signed = w4_velocity_payload(
            secret.as_bytes(),
            "203.0.113.10",
            fwd_uuid,
            "W4Velocity",
            &[ForwardedProperty {
                name: "textures".into(),
                value: "dmFs".into(),
                signature: Some("c2ln".into()),
            }],
        );
        let (mut cli, server) = w4_client();
        w4_login_start(&mut cli, "Impostor_Name", [0x11u8; 16]);
        let mid = w4_expect_plugin_request(&mut cli, deadline);
        w4_send_plugin_response(&mut cli, mid, &signed);
        let body = w4_read_until(&mut cli, 0x02, deadline).expect("login success");
        let mut b = &body[..];
        assert_eq!(&b[..16], &fwd_uuid, "forwarded uuid must be adopted");
        b = &b[16..];
        let n = usize::try_from(read_varint(&mut b).unwrap()).unwrap();
        assert_eq!(&b[..n], b"W4Velocity", "forwarded name must be adopted");
        drop(cli);
        let _ = server.join();

        // The IP ban runs on the *forwarded* address, not the loopback peer.
        *live_admission().lock().unwrap() = Some(LiveAdmission {
            ops: OpsRegistry::default(),
            bans: BanRegistry {
                players: Vec::new(),
                ips: vec![IpBanEntry {
                    ip: "203.0.113.99".into(),
                    created: "test".into(),
                    source: "test".into(),
                    expires: "forever".into(),
                    reason: "test ban".into(),
                }],
            },
            whitelist: WhitelistRegistry::default(),
            max_players: 20,
        });
        let signed = w4_velocity_payload(
            secret.as_bytes(),
            "203.0.113.99",
            [0xbcu8; 16],
            "W4BannedIp",
            &[],
        );
        let (mut cli, server) = w4_client();
        w4_login_start(&mut cli, "LoopbackOk", [0x22u8; 16]);
        let mid = w4_expect_plugin_request(&mut cli, deadline);
        w4_send_plugin_response(&mut cli, mid, &signed);
        let body = w4_read_until(&mut cli, 0x00, deadline).expect("ip ban disconnect");
        let mut sl = &body[..];
        let n = usize::try_from(read_varint(&mut sl).unwrap()).unwrap();
        let text = std::str::from_utf8(&sl[..n]).unwrap().to_string();
        assert!(
            text.contains("Your IP address is banned from this server."),
            "{text}"
        );
        drop(cli);
        let _ = server.join();
        *live_admission().lock().unwrap() = None;

        // A tampered payload fails closed with a Login Disconnect, then the
        // connection is gone entirely.
        let mut bad = w4_velocity_payload(
            secret.as_bytes(),
            "203.0.113.10",
            [0xccu8; 16],
            "W4Tamperer",
            &[],
        );
        let last = bad.len() - 1;
        bad[last] ^= 0x80;
        let (mut cli, server) = w4_client();
        w4_login_start(&mut cli, "W4Tamperer", [0x33u8; 16]);
        let mid = w4_expect_plugin_request(&mut cli, deadline);
        w4_send_plugin_response(&mut cli, mid, &bad);
        let (pid, _body) = w4_read_packet(&mut cli, deadline).expect("fail-closed disconnect");
        assert_eq!(pid, 0x00, "a tampered forward must disconnect");
        assert!(
            w4_read_packet(&mut cli, deadline).is_none(),
            "the connection must be closed after the disconnect"
        );
        drop(cli);
        let _ = server.join();
    }

    #[test]
    fn w4_encrypted_stream_roundtrips_protocol_packets_with_aes128_cfb8() {
        let _env = W4Env::new().set("MC_SPINE_ONLINE_MODE", "1");
        let deadline = w4_deadline();
        let secret = [0x5au8; 16];
        let key_der = vec![0x30, 0x82, 0x01, 0x0a];
        let auth = Arc::new(W4Auth {
            key_der: key_der.clone(),
            token: vec![0xde, 0xad, 0xbe, 0xef],
            profile: MojangJoinedProfile {
                uuid: [0x42u8; 16],
                name: "W4Verified".into(),
                properties: vec![ForwardedProperty {
                    name: "textures".into(),
                    value: "dmFs".into(),
                    signature: Some("c2ln".into()),
                }],
            },
            decrypt_calls: Mutex::new(Vec::new()),
            seen_hash: Mutex::new(None),
            seen_ip: Mutex::new(None),
        });
        set_online_auth_provider(Some(auth.clone()));

        let (mut cli, server) = w4_client();
        w4_login_start(&mut cli, "W4Plain", [0x33u8; 16]);

        // Encryption Request: the provider's key and verify token go out plain.
        let body = w4_read_until(&mut cli, 0x01, deadline).expect("encryption request");
        let (server_id, req_key, req_token, should_auth) =
            decode_encryption_request(&body).expect("request decodes");
        assert_eq!(server_id, "");
        assert_eq!(req_key, key_der);
        assert_eq!(req_token, vec![0xde, 0xad, 0xbe, 0xef]);
        assert!(should_auth);

        // Encryption Response ("encrypted" is the identity map under the test
        // provider), then both ends switch to AES-128-CFB8.
        w4_send(&mut cli, 0x01, &encode_encryption_response(&secret, &req_token));
        cli.enable_encryption(&secret).unwrap();

        // Login Success arrives encrypted, carrying the session profile.
        let body = w4_read_until(&mut cli, 0x02, deadline).expect("login success");
        let mut b = &body[..];
        assert_eq!(&b[..16], &[0x42u8; 16], "profile uuid must be adopted");
        b = &b[16..];
        let n = usize::try_from(read_varint(&mut b).unwrap()).unwrap();
        assert_eq!(&b[..n], b"W4Verified", "profile name must be adopted");

        // Login Acknowledged + the configuration dance, all encrypted. The
        // server only advances if these serverbound frames decode.
        w4_send(&mut cli, 0x03, &[]);
        let mut ci = Vec::new();
        write_str(&mut ci, "en_us");
        ci.push(2);
        write_varint(&mut ci, 0);
        ci.push(1);
        ci.push(0x7f);
        write_varint(&mut ci, 1);
        ci.push(0);
        ci.push(1);
        w4_send(&mut cli, 0x00, &ci);
        let mut finished = false;
        while let Some((pid, _)) = w4_read_packet(&mut cli, deadline) {
            if pid == 0x0e {
                let mut kp = Vec::new();
                write_varint(&mut kp, 0);
                w4_send(&mut cli, 0x07, &kp);
            } else if pid == 0x03 {
                w4_send(&mut cli, 0x03, &[]);
                finished = true;
                break;
            }
        }
        assert!(finished, "finish configuration under encryption");

        // The first Play packet family — Join Game 0x2b — must decrypt too.
        assert!(
            w4_read_until(&mut cli, 0x2b, deadline).is_some(),
            "join game under AES-128-CFB8"
        );

        // The session check saw the vanilla server hash and the real peer IP.
        assert_eq!(
            *auth.seen_hash.lock().unwrap(),
            Some(mc_server_hash("", &secret, &key_der)),
            "hasJoined must be asked for the sha1 of id || secret || key"
        );
        assert_eq!(
            auth.seen_ip.lock().unwrap().as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(auth.decrypt_calls.lock().unwrap().len(), 2);
        drop(cli);
        let _ = server.join();

        // Fail closed: online mode enabled without an installed provider
        // refuses the login rather than silently running offline.
        set_online_auth_provider(None);
        let (mut cli, server) = w4_client();
        w4_login_start(&mut cli, "W4NoAuth", [0x44u8; 16]);
        let (pid, _body) = w4_read_packet(&mut cli, deadline).expect("disconnect");
        assert_eq!(pid, 0x00, "no provider must fail closed");
        drop(cli);
        let _ = server.join();
    }

    #[test]
    fn w4_isolated_connection_catches_panic_without_poisoning_server() {
        // A registered, mounted client that a panicking handler would orphan.
        let (client, _peer) = effect_tests::client_pair();
        let me = Arc::new(client);
        clients().lock().unwrap().push(me.clone());
        let vehicle = NEXT_EID.fetch_add(1, Ordering::SeqCst);
        {
            let mut live = mounts().lock().unwrap();
            live.registry
                .register_vehicle(vehicle, VehicleKind::Boat, [0.0, 64.0, 0.0]);
            live.registry.register_rider(me.eid, [0.0, 64.0, 0.0]);
            live.registry.mount(vehicle, me.eid).unwrap();
        }
        assert_eq!(vehicle_of(me.eid), Some(vehicle));

        let before = ISOLATED_CONNECTION_DISCONNECTS.load(Ordering::SeqCst);
        let out: Option<u8> = run_isolated_connection(
            "w4-panic-test",
            || -> u8 { panic!("synthetic handler bug") },
            || forget_live_client(me.eid),
        );
        assert_eq!(out, None);
        assert_eq!(
            ISOLATED_CONNECTION_DISCONNECTS.load(Ordering::SeqCst),
            before + 1
        );
        assert!(
            clients().lock().unwrap().iter().all(|c| c.eid != me.eid),
            "the panicked client must be unregistered"
        );
        assert_eq!(vehicle_of(me.eid), None, "the ghost rider must dismount");

        // The process is not poisoned: a fresh loopback login still completes.
        let (mut cli, server) = w4_client();
        w4_login_start(&mut cli, "W4Survivor", [0x99u8; 16]);
        assert!(
            w4_read_until(&mut cli, 0x02, w4_deadline()).is_some(),
            "server still answers logins after an isolated panic"
        );
        drop(cli);
        let _ = server.join();
    }
}

// ---------------- Lane P5c: live-path wiring tests ----------------

#[cfg(test)]
mod p5c_live_tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    /// Restores every `world()` cell a fixture wrote, in reverse order.
    struct Cells(Vec<(i32, i32, i32, u32)>);
    impl Cells {
        fn new() -> Self {
            Cells(Vec::new())
        }
        fn set(&mut self, x: i32, y: i32, z: i32, st: u32) {
            self.0.push((x, y, z, block_at(x, y, z)));
            set_block(x, y, z, st);
        }
        fn record(&mut self, x: i32, y: i32, z: i32) {
            self.0.push((x, y, z, block_at(x, y, z)));
        }
    }
    impl std::ops::Drop for Cells {
        fn drop(&mut self) {
            for (x, y, z, st) in self.0.drain(..).rev() {
                set_block(x, y, z, st);
            }
        }
    }

    /// Restores the gamerules a test touched, on every exit path.
    struct Rules(Vec<(&'static str, i32)>);
    impl Rules {
        fn take(names: &[&'static str]) -> Self {
            let rules = command_gamerules().lock().unwrap();
            let taken = names
                .iter()
                .map(|name| (*name, rules.get(name).copied().unwrap_or(-1)))
                .collect::<Vec<_>>();
            Rules(taken)
        }
        fn set(&self, name: &'static str, value: i32) {
            command_gamerules().lock().unwrap().insert(name, value);
        }
    }
    impl std::ops::Drop for Rules {
        fn drop(&mut self) {
            let mut rules = command_gamerules().lock().unwrap();
            for (name, value) in self.0.drain(..) {
                rules.insert(name, value);
            }
        }
    }

    /// A loopback client for wire-path tests: `me.sock` is the end the server
    /// writes to; the returned peer reads exactly those bytes.
    fn loopback_client(name: &str, eid: i32) -> (Arc<Client>, std::net::TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = std::net::TcpStream::connect(addr).unwrap();
        let (peer, _) = listener.accept().unwrap();
        let me = Arc::new(Client {
            eid,
            uuid: [0x5cu8; 16],
            name: name.to_string(),
            player_attack_damage_id: None,
            pos: Mutex::new((0.5, 65.0, 0.5, 0.0, 0.0, true)),
            teleport: Mutex::new(Default::default()),
            hp: Mutex::new(20.0),
            sock: Mutex::new(stream),
            inv: Mutex::new(HashMap::new()),
            cursor: Mutex::new((0, 0, Vec::new())),
            drag: Mutex::new((0, Vec::new())),
            open_chest: Mutex::new(None),
            chest_sync: Mutex::new(None),
            open_furnace: Mutex::new(None),
            craft: Mutex::new(HashMap::new()),
            craft9: Mutex::new(HashMap::new()),
            open_table: Mutex::new(false),
            open_merchant: Mutex::new(false),
            trade_sel: Mutex::new(0),
            merchant_in: Mutex::new([(0, 0, Vec::new()), (0, 0, Vec::new())]),
            bow_draw: Mutex::new(None),
            dig: Mutex::new(None),
            eat_start: Mutex::new(None),
            blocking: Mutex::new(false),
            sprinting: Mutex::new(false),
            sneaking: Mutex::new(false),
            last_melee: Mutex::new(Instant::now() - Duration::from_secs(10)),
            fall_fly: Mutex::new(false),
            invulnerable: Mutex::new(0),
            last_hurt: Mutex::new(0.0),
            shield_cd: Mutex::new(Instant::now() - Duration::from_secs(10)),
            absorb: Mutex::new(0.0),
            pearl_cd: Mutex::new(Instant::now() - Duration::from_secs(10)),
            xp: Mutex::new(0),
            skin: Mutex::new(None),
            seen: Mutex::new(HashSet::new()),
            chunks: Mutex::new(HashSet::from([(-1, -1), (-1, 0), (0, -1), (0, 0)])),
            chunk_ack: Mutex::new(ChunkFeedback::new(1)),
            gen_sources: Mutex::new(GenSources::default()),
            gm: Mutex::new(0),
            fall_from: Mutex::new(65.0),
            food: Mutex::new(20),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
            armor_pts: Mutex::new(0),
            sat: Mutex::new(5.0),
            walk_acc: Mutex::new(0.0),
            lastp: Mutex::new((0.5, 65.0, 0.5)),
            spawn_pt: Mutex::new(None),
            dim: Mutex::new(DimId::Overworld),
            spawn_dim: Mutex::new(DimId::Overworld),
            view: Mutex::new(2),
            in_portal: Mutex::new(false),
            portal_at: Mutex::new(None),
            held: Mutex::new(0),
            effs: Mutex::new(Vec::new()),
            lod_v1_enabled: AtomicBool::new(false),
            underground_revealed: AtomicBool::new(false),
            sleeping: Mutex::new(None),
            inv_state_id: AtomicI32::new(0),
        });
        (me, peer)
    }

    /// Removes the test client from the live list and restores `DIFFICULTY`.
    struct LiveCleanup {
        eid: i32,
        difficulty: u8,
    }
    impl std::ops::Drop for LiveCleanup {
        fn drop(&mut self) {
            clients()
                .lock()
                .unwrap()
                .retain(|c| c.eid != self.eid);
            DIFFICULTY.store(self.difficulty, Ordering::Relaxed);
        }
    }

    /// Read one framed server packet from the client peer, or None on timeout.
    fn read_frame(peer: &mut std::net::TcpStream) -> Option<(i64, Vec<u8>)> {
        peer.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).ok()?;
        let mut body = &buf[..n];
        let _len = read_varint(&mut body)?;
        let id = read_varint(&mut body)?;
        Some((id, body.to_vec()))
    }

    #[test]
    fn p5c_live_random_tick_dim_dispatches_p2_plants() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _rules = Rules::take(&["doVinesSpread"]);
        let reg = crate::block_registry::breg().unwrap();
        let (cactus, sapling, kelp, vine) = (
            reg.state_of("minecraft:cactus", None),
            reg.state_of("minecraft:bamboo_sapling", None),
            reg.state_of("minecraft:kelp", None),
            reg.state_of("minecraft:vine", None),
        );
        if reg.name_of(cactus) != "minecraft:cactus"
            || reg.name_of(sapling) != "minecraft:bamboo_sapling"
            || reg.name_of(kelp) != "minecraft:kelp"
            || reg.name_of(vine) != "minecraft:vine"
        {
            return;
        }
        let mut cells = Cells::new();

        // Cactus at age 15 grows a fresh segment through the live dispatch —
        // no roll involved.
        cells.set(0, 199, 0, reg.state_of("minecraft:sand", None));
        cells.set(0, 200, 0, reg.with_prop(cactus, "age", "15"));
        cells.record(0, 201, 0);
        on_random_tick_dim(reg, DimId::Overworld, 0, 200, 0);
        assert_eq!(
            reg.name_of(block_at(0, 201, 0)),
            "minecraft:cactus",
            "on_random_tick_dim must reach cactus_random_tick"
        );

        // The bamboo_sapling arm must outrank the `_sapling` suffix arm: the
        // sapling becomes a bamboo stalk, not a tree-grow tick.
        cells.set(4, 199, 0, reg.state_of("minecraft:dirt", None));
        cells.set(4, 200, 0, sapling);
        let mut stalk = false;
        for _ in 0..64 {
            on_random_tick_dim(reg, DimId::Overworld, 4, 200, 0);
            if reg.name_of(block_at(4, 200, 0)) == "minecraft:bamboo" {
                stalk = true;
                break;
            }
        }
        assert!(stalk, "on_random_tick_dim must reach bamboo_random_tick");

        // Kelp head under a water source grows into the water cell.
        cells.set(8, 200, 0, reg.with_prop(kelp, "age", "5"));
        cells.set(8, 201, 0, reg.state_of("minecraft:water", None));
        on_random_tick_dim(reg, DimId::Overworld, 8, 200, 0);
        assert_eq!(
            reg.name_of(block_at(8, 201, 0)),
            "minecraft:kelp",
            "on_random_tick_dim must reach kelp_random_tick"
        );
        assert_eq!(reg.name_of(block_at(8, 200, 0)), "minecraft:kelp_plant");

        // doVinesSpread=true: a hanging vine spreads down on the one-in-four
        // roll — 64 dispatch passes make the roll effectively certain.
        _rules.set("doVinesSpread", 1);
        cells.set(12, 200, 0, vine);
        cells.record(12, 199, 0);
        let mut spread = false;
        for _ in 0..64 {
            on_random_tick_dim(reg, DimId::Overworld, 12, 200, 0);
            if reg.name_of(block_at(12, 199, 0)) == "minecraft:vine" {
                spread = true;
                break;
            }
        }
        assert!(spread, "on_random_tick_dim must reach vine_random_tick");
    }

    #[test]
    fn p5c_live_player_equipment_wire_round_trip() {
        crate::test_support::require_vanilla_data();
        let (me, _peer) = loopback_client("GearCheck", 42001);
        let (sword, shield, boots, pants, chest, helm) = (
            name_to_item("iron_sword").unwrap(),
            name_to_item("shield").unwrap(),
            name_to_item("iron_boots").unwrap(),
            name_to_item("iron_leggings").unwrap(),
            name_to_item("iron_chestplate").unwrap(),
            name_to_item("iron_helmet").unwrap(),
        );
        {
            let mut inv = me.inv.lock().unwrap();
            inv.insert(36, (sword, 1, Vec::new())); // selected hotbar
            inv.insert(45, (shield, 1, Vec::new())); // offhand
            inv.insert(5, (helm, 1, Vec::new()));
            inv.insert(6, (chest, 1, Vec::new()));
            inv.insert(7, (pants, 1, Vec::new()));
            inv.insert(8, (boots, 1, Vec::new()));
        }
        let eq = player_equipment(&me);
        let packet = player_equipment_packet(&me).expect("six worn slots encode a packet");
        let (eid, entries) = decode_set_equipment(&packet).expect("decodes");
        assert_eq!(eid, me.eid);
        let by_slot: HashMap<u8, Stack> = entries.into_iter().collect();
        assert_eq!(by_slot.len(), 6, "mainhand+offhand+4 armor on the wire");
        assert_eq!(by_slot[&0].0, sword);
        assert_eq!(by_slot[&1].0, shield);
        assert_eq!(by_slot[&2].0, boots);
        assert_eq!(by_slot[&3].0, pants);
        assert_eq!(by_slot[&4].0, chest);
        assert_eq!(by_slot[&5].0, helm);
        assert_eq!(eq.slots[EquipmentSlot::MainHand as usize].0, sword);

        // The diff encoder emits only changed slots — and an emptied slot
        // still rides the wire, which is how trackers see a piece come off.
        let mut after = EntityEquipment::default();
        after.slots = eq.slots.clone();
        after.slots[EquipmentSlot::Head as usize] = (0, 0, Vec::new());
        let packet = encode_set_equipment_changes(me.eid, &eq, &after).expect("one change");
        let (eid, entries) = decode_set_equipment(&packet).expect("decodes");
        assert_eq!(eid, me.eid);
        assert_eq!(entries, vec![(5u8, (0, 0, Vec::new()))]);
        assert!(encode_set_equipment_changes(me.eid, &after, &after).is_none());
    }

    #[test]
    fn p5c_live_teleport_confirmation_tracks_current_id() {
        // #123: only the id of the currently-sent teleport unblocks movement;
        // a stale or mismatched confirmation is ignored.
        let mut sync = connection_limits::TeleportSync::default();
        assert!(sync.movement_allowed());
        let first = sync.begin();
        let second = sync.begin();
        assert_ne!(first, second);
        assert!(sync.preparing(second));
        sync.sent();
        sync.confirm(first);
        assert!(
            !sync.movement_allowed(),
            "a stale teleport id must not unblock movement"
        );
        sync.confirm(second + 1);
        assert!(!sync.movement_allowed(), "a wrong id must not confirm");
        sync.confirm(second);
        assert!(sync.movement_allowed(), "the current sent id confirms");
    }

    #[test]
    fn p5c_live_difficulty_op_gate_and_broadcast() {
        let _guard = world_guard();
        let (me, mut peer) = loopback_client("OpCheck", 42002);
        let _cleanup = LiveCleanup {
            eid: me.eid,
            difficulty: DIFFICULTY.load(Ordering::Relaxed),
        };
        clients().lock().unwrap().push(me.clone());

        // #131: hasPermission(2) — a non-operator is refused before the
        // difficulty ever changes; a level-4 operator is let through.
        assert!(!player_is_admin(&me.name), "default players are not operators");
        OPERATORS
            .lock()
            .unwrap()
            .insert(me.name.to_ascii_lowercase());
        assert!(player_is_admin(&me.name));
        OPERATORS.lock().unwrap().remove(&me.name.to_ascii_lowercase());

        set_difficulty(0);
        assert_eq!(DIFFICULTY.load(Ordering::Relaxed), 0);
        let (id, body) = read_frame(&mut peer).expect("0x0b broadcast lands");
        assert_eq!(id, 0x0b);
        assert_eq!(body, vec![0u8, 0u8], "difficulty 0, unlocked");
        set_difficulty(5);
        assert_eq!(DIFFICULTY.load(Ordering::Relaxed), 0, "out of range refused");
    }

    #[test]
    fn p5c_live_end_and_nether_furnaces_tick() {
        let _guard = world_guard();
        let input = name_to_item("raw_iron").unwrap();
        let coal = name_to_item("coal").unwrap();
        test_region_warm_insert((0, 0));
        furnaces().lock().unwrap().clear();
        for dim in [DimId::Nether, DimId::End] {
            furnaces().lock().unwrap().insert(
                (dim, 0, 70, 0),
                Furnace {
                    input: (input, 1, Vec::new()),
                    fuel: (coal, 1, Vec::new()),
                    output: (0, 0, Vec::new()),
                    burn_left: 0,
                    progress: 0,
                    dirty: false,
                    xp: 0.0,
                    recipe_cache: CachedLookup::default(),
                    fuel_cache: CachedLookup::default(),
                },
            );
        }
        furnace_tick();
        let guard = furnaces().lock().unwrap();
        for dim in [DimId::Nether, DimId::End] {
            let furnace = guard
                .get(&(dim, 0, 70, 0))
                .unwrap_or_else(|| panic!("{dim:?} furnace must keep its key"));
            assert_eq!(furnace.progress, 1, "{dim:?} furnace must tick");
        }
        drop(guard);
        furnaces().lock().unwrap().clear();
        test_region_warm_remove(&(0, 0));
    }

    #[test]
    fn p5c_live_adventure_can_break_and_place_on_predicates() {
        crate::test_support::require_vanilla_data();
        let reg = crate::block_registry::breg().unwrap();
        let stone = reg.state_of("minecraft:stone", None);
        let dirt = reg.state_of("minecraft:dirt", None);
        if reg.name_of(stone) != "minecraft:stone" || reg.name_of(dirt) != "minecraft:dirt" {
            return;
        }
        let stone_id = reg.block_id_of(stone).unwrap();
        // predicates = 1 entry: holder-set { stone }, no properties, no NBT.
        let mut data = Vec::new();
        write_varint(&mut data, 1);
        data.push(1); // blocks present
        write_varint(&mut data, 1);
        write_varint(&mut data, i64::from(stone_id));
        data.push(0); // no property matchers
        data.push(0); // no NBT
        let held: Stack = (name_to_item("diamond_pickaxe").unwrap(), 1, vec![
            (COMP_CAN_BREAK, data.clone()),
            (COMP_CAN_PLACE_ON, data),
        ]);
        assert!(adventure_stack_allows(reg, stone, &held, COMP_CAN_BREAK));
        assert!(!adventure_stack_allows(reg, dirt, &held, COMP_CAN_BREAK));
        assert!(adventure_stack_allows(reg, stone, &held, COMP_CAN_PLACE_ON));
        assert!(!adventure_stack_allows(reg, dirt, &held, COMP_CAN_PLACE_ON));

        // A stack without the component allows nothing; one unconstrained
        // predicate allows every block.
        let bare: Stack = (held.0, 1, Vec::new());
        assert!(!adventure_stack_allows(reg, stone, &bare, COMP_CAN_BREAK));
        let mut open = Vec::new();
        write_varint(&mut open, 1);
        open.extend_from_slice(&[0, 0, 0]);
        let any: Stack = (held.0, 1, vec![(COMP_CAN_BREAK, open)]);
        assert!(adventure_stack_allows(reg, stone, &any, COMP_CAN_BREAK));
        assert!(adventure_stack_allows(reg, dirt, &any, COMP_CAN_BREAK));
    }

    #[test]
    fn p5c_live_trapdoor_wooden_toggles_iron_stays_put() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let reg = crate::block_registry::breg().unwrap();
        let oak = reg.state_of("minecraft:oak_trapdoor", None);
        let iron = reg.state_of("minecraft:iron_trapdoor", None);
        if reg.name_of(oak) != "minecraft:oak_trapdoor"
            || reg.name_of(iron) != "minecraft:iron_trapdoor"
        {
            return;
        }
        let mut cells = Cells::new();

        cells.set(0, 100, 0, oak);
        let before = reg.get_prop(block_at(0, 100, 0), "open");
        assert!(
            try_toggle_trapdoor(DimId::Overworld, 0, 100, 0),
            "wooden trapdoor must answer a right-click"
        );
        let after = reg.get_prop(block_at(0, 100, 0), "open");
        assert_ne!(before, after, "the open property must flip");
        assert!(try_toggle_trapdoor(DimId::Overworld, 0, 100, 0));
        assert_eq!(reg.get_prop(block_at(0, 100, 0), "open"), before);

        cells.set(4, 100, 0, iron);
        let iron_before = block_at(4, 100, 0);
        assert!(
            !try_toggle_trapdoor(DimId::Overworld, 4, 100, 0),
            "iron trapdoor is redstone-only"
        );
        assert_eq!(block_at(4, 100, 0), iron_before);
    }

    #[test]
    fn p5c_live_light_edits_coalesce_to_one_batch_entry() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let reg = crate::block_registry::breg().unwrap();
        let glow = reg.state_of("minecraft:glowstone", None);
        if reg.name_of(glow) != "minecraft:glowstone" {
            return;
        }
        pending_light_edits().lock().unwrap().clear();
        let pos = Position { x: 5, y: 70, z: 5 };
        // Two edits to the same cell inside one tick collapse into a single
        // queued entry carrying the first old and the last new — the drain
        // then feeds ONE edit_batch per dimension.
        note_light_edit(DimId::Overworld, pos, glow, 0);
        note_light_edit(DimId::Overworld, pos, 0, glow);
        let edits = pending_light_edits().lock().unwrap();
        let per_dim = edits.get(&DimId::Overworld).expect("queued");
        assert_eq!(per_dim.len(), 1, "same-cell edits coalesce");
        let (old, new) = per_dim.get(&pos).copied().unwrap();
        drop(edits);
        assert_eq!(old, light_properties(reg, glow));
        assert_eq!(new, light_properties(reg, glow));
        pending_light_edits().lock().unwrap().clear();
    }

    #[test]
    fn p5c_live_pick_item_plan_selects_swaps_or_fills() {
        let stone = name_to_item("stone").unwrap();
        let mut inv = HashMap::new();
        // Hotbar hit: the slot is merely selected.
        inv.insert(38i16, (stone, 3, Vec::new()));
        assert_eq!(
            pick_item_plan(&inv, stone, false),
            Some(PickItemPlan::Select(38))
        );
        // Main-inventory hit swaps into the selected slot.
        let mut inv = HashMap::new();
        inv.insert(20i16, (stone, 3, Vec::new()));
        assert_eq!(
            pick_item_plan(&inv, stone, false),
            Some(PickItemPlan::Swap(20))
        );
        // No hit: only creative invents the stack.
        let inv = HashMap::new();
        assert_eq!(pick_item_plan(&inv, stone, false), None);
        assert_eq!(
            pick_item_plan(&inv, stone, true),
            Some(PickItemPlan::FillCreative)
        );
        // A component-bearing stack does not match a plain block pick.
        let mut inv = HashMap::new();
        inv.insert(38i16, (stone, 3, vec![(COMP_CUSTOM_NAME, vec![0])]));
        assert_eq!(pick_item_plan(&inv, stone, false), None);
    }
    // ---------- Lane W1: proofs the built evaluators run through the live loop ----------

    /// Protocol-767 C2S `0x06` body: string, timestamp, salt, optional
    /// signature, last-seen count and the three-byte acknowledged bit set.
    fn w1_chat_body(message: &str, signed: bool) -> Vec<u8> {
        let mut b = Vec::new();
        write_str(&mut b, message);
        b.extend_from_slice(&1i64.to_be_bytes());
        b.extend_from_slice(&2i64.to_be_bytes());
        if signed {
            b.push(1);
            b.extend_from_slice(&[7u8; 256]);
        } else {
            b.push(0);
        }
        write_varint(&mut b, 0i64);
        b.extend_from_slice(&[0u8; 3]);
        b
    }

    #[test]
    fn w1_live_wire_signed_chat_relays_0x39_and_0x1e() {
        let _guard = world_guard();
        let (sender, _sender_peer) = loopback_client("w1sender", 0x7101);
        let (recv, mut recv_peer) = loopback_client("w1recv", 0x7102);
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(sender.clone());
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(recv.clone());
        let _c1 = LiveCleanup {
            eid: sender.eid,
            difficulty: DIFFICULTY.load(Ordering::Relaxed),
        };
        let _c2 = LiveCleanup {
            eid: recv.eid,
            difficulty: DIFFICULTY.load(Ordering::Relaxed),
        };
        let mut throttle = connection_limits::ChatThrottle::new(Instant::now());
        // A signed envelope relays as protocol-767 Player Chat; the wire
        // client only sees the packet id, which pins the `0x39` path.
        live_chat_message(&sender, &w1_chat_body("w1 signed", true), &mut throttle)
            .expect("signed envelope must pass admission");
        let (id, _) = read_frame(&mut recv_peer).expect("receiver must get a chat frame");
        assert_eq!(id, S2C_PLAYER_CHAT_767, "signed chat must relay as 0x39");
        // Offline mode allows the unsigned envelope through the vanilla
        // Disguised Chat relay.
        live_chat_message(&sender, &w1_chat_body("w1 plain", false), &mut throttle)
            .expect("unsigned offline envelope must pass admission");
        let (id, _) = read_frame(&mut recv_peer).expect("receiver must get a chat frame");
        assert_eq!(id, S2C_DISGUISED_CHAT_767, "unsigned chat must relay as 0x1e");
    }

    #[test]
    fn w1_live_wire_vehicle_tick_moves_railed_and_ridden() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let reg = crate::block_registry::breg().unwrap();
        let (me, _peer) = loopback_client("w1rider", 0x7201);
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(me.clone());
        let _clean = LiveCleanup {
            eid: me.eid,
            difficulty: DIFFICULTY.load(Ordering::Relaxed),
        };
        let mut cells = Cells::new();

        // A minecart on a powered rail: the `vehicles` game_tick phase must
        // run the wm_minecart evaluators against its live state.
        let rail = reg.with_prop(
            reg.with_prop(
                reg.state_of("minecraft:powered_rail", None),
                "powered",
                "true",
            ),
            "shape",
            "east_west",
        );
        cells.set(2, 64, 8, rail);
        cells.set(3, 64, 8, reg.state_of("minecraft:stone", None));
        assert!(insert_live_mob(
            69,
            DimId::Overworld,
            (2.5, 64.0, 8.5, 0.0, 0.0),
            6.0,
            None
        ));
        let cart_eid = mobs().lock().unwrap_or_else(|e| e.into_inner()).last().unwrap().eid;
        w1_vehicle_phys()
            .lock()
            .unwrap()
            .insert(cart_eid, W1VehiclePhys {
                vel: [0.1, 0.0, 0.0],
                ..W1VehiclePhys::default()
            });

        // A boat afloat with a mounted, paddling driver.
        cells.set(6, 64, 8, reg.state_of("minecraft:water", None));
        assert!(insert_live_mob(
            10,
            DimId::Overworld,
            (6.5, 64.6, 8.5, 0.0, 0.0),
            6.0,
            None
        ));
        let boat_eid = mobs().lock().unwrap_or_else(|e| e.into_inner()).last().unwrap().eid;
        assert!(try_mount_vehicle(&me, boat_eid));
        {
            let mut live = mounts().lock().unwrap_or_else(|e| e.into_inner());
            live.paddling.insert(
                boat_eid,
                PaddleBoatPacket {
                    left: true,
                    right: true,
                },
            );
            live.steering.insert(
                boat_eid,
                PlayerInputPacket {
                    sideways: 0.0,
                    forward: 1.0,
                    jump: false,
                    unmount: false,
                },
            );
        }

        game_tick(|name, tick| {
            if matches!(name, "hot_regions" | "vehicles") {
                tick();
            }
        });
        let cart = mobs()
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.eid == cart_eid)
            .unwrap()
            .clone();
        let cart_pos = *cart.pos.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            (cart_pos.0 - 2.5).abs() > 0.001,
            "minecart on a live powered rail must move (pos = {cart_pos:?})"
        );
        assert!(
            w1_vehicle_phys().lock().unwrap_or_else(|e| e.into_inner()).contains_key(&cart_eid),
            "vehicle tick must register physics state for the cart"
        );
        let boat = mobs()
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.eid == boat_eid)
            .unwrap()
            .clone();
        let boat_pos = *boat.pos.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            (boat_pos.0 - 6.5).abs() > 0.001 || (boat_pos.2 - 8.5).abs() > 0.001,
            "paddled boat must move (pos = {boat_pos:?})"
        );
        player_dismount(&me, false);
        remove_live_mob(cart_eid);
        remove_live_mob(boat_eid);
        live_mount_forget(boat_eid);
        live_mount_forget(cart_eid);
    }

    #[test]
    fn w1_live_wire_hostile_mob_ai_dispatches_in_mob_tick() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _clean = LiveCleanup {
            eid: 0x7301,
            difficulty: DIFFICULTY.load(Ordering::Relaxed),
        };
        DIFFICULTY.store(3, Ordering::Relaxed);
        let (me, _peer) = loopback_client("w1prey", 0x7301);
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(me.clone());
        let arrows0 = arrows().lock().unwrap_or_else(|e| e.into_inner()).len();

        // One member of every family `MobBehaviorState::for_etype` lacks:
        // pillager 80, phantom 76, evoker 35, guardian 50.
        let before: HashSet<i32> = mobs().lock().unwrap_or_else(|e| e.into_inner()).iter().map(|m| m.eid).collect();
        for (etype, pos) in [
            (80i64, (8.5, 65.0, 0.5, 0.0f32, 0.0f32)),
            (76, (4.5, 72.0, 0.5, 0.0, 0.0)),
            (35, (10.5, 65.0, 0.5, 0.0, 0.0)),
            (50, (-6.5, 65.0, 0.5, 0.0, 0.0)),
        ] {
            assert!(insert_live_mob(etype, DimId::Overworld, pos, 200.0, None));
        }
        let eids: Vec<i32> = mobs()
            .lock()
            .unwrap()
            .iter()
            .filter(|m| !before.contains(&m.eid))
            .map(|m| m.eid)
            .collect();
        assert_eq!(eids.len(), 4);
        let phantom_eid = mobs()
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.etype == 76 && eids.contains(&m.eid))
            .unwrap()
            .eid;

        game_tick(|name, tick| {
            if name == "hot_regions" {
                tick();
            }
        });
        for _ in 0..40 {
            game_tick(|name, tick| {
                if name == "mobs" {
                    tick();
                }
            });
        }

        let states = w1_live_mob_ai().lock().unwrap_or_else(|e| e.into_inner());
        for &eid in &eids {
            assert!(
                states.contains_key(&eid),
                "mob eid {eid} must reach the specialized dispatch"
            );
        }
        drop(states);
        assert!(
            arrows().lock().unwrap_or_else(|e| e.into_inner()).len() > arrows0,
            "pillager must fire a live arrow from inside mob_tick"
        );
        let phantom = mobs()
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.eid == phantom_eid)
            .unwrap()
            .clone();
        let p = *phantom.pos.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            (p.0 - 4.5).abs() > 0.001 || (p.1 - 72.0).abs() > 0.001 || (p.2 - 0.5).abs() > 0.001,
            "phantom behavior must own its live motion (pos = {p:?})"
        );
        for eid in eids {
            w1_live_mob_ai().lock().unwrap_or_else(|e| e.into_inner()).remove(&eid);
            remove_live_mob(eid);
        }
    }

    #[test]
    fn w1_live_wire_portal_delay_gamerules_gate_enter_portal() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _rules = Rules::take(&[
            "playersNetherPortalDefaultDelay",
            "playersNetherPortalCreativeDelay",
        ]);
        let reg = crate::block_registry::breg().unwrap();
        let (me, _peer) = loopback_client("w1portal", 0x7401);
        let mut cells = Cells::new();
        cells.set(0, 65, 0, reg.state_of("minecraft:nether_portal", None));

        // A nonzero delay starts the wait instead of entering.
        _rules.set("playersNetherPortalDefaultDelay", 5);
        assert!(!tick_portal(&me, 2));
        assert!(me.portal_at.lock().unwrap_or_else(|e| e.into_inner()).is_some());
        assert_eq!(*me.dim.lock().unwrap_or_else(|e| e.into_inner()), DimId::Overworld);

        // A zero delay enters on the same call.
        *me.in_portal.lock().unwrap_or_else(|e| e.into_inner()) = false;
        *me.portal_at.lock().unwrap_or_else(|e| e.into_inner()) = None;
        _rules.set("playersNetherPortalDefaultDelay", 0);
        assert!(tick_portal(&me, 2));
        assert_eq!(*me.dim.lock().unwrap_or_else(|e| e.into_inner()), DimId::Nether);

        // The creative rule is a separate key: a creative player still waits
        // when only the default delay is zero.
        *me.dim.lock().unwrap_or_else(|e| e.into_inner()) = DimId::Overworld;
        *me.in_portal.lock().unwrap_or_else(|e| e.into_inner()) = false;
        *me.portal_at.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *me.pos.lock().unwrap_or_else(|e| e.into_inner()) = (0.5, 65.0, 0.5, 0.0, 0.0, true);
        *me.gm.lock().unwrap_or_else(|e| e.into_inner()) = 1;
        _rules.set("playersNetherPortalCreativeDelay", 5);
        assert!(!tick_portal(&me, 2));
        assert_eq!(*me.dim.lock().unwrap_or_else(|e| e.into_inner()), DimId::Overworld);
        *me.gm.lock().unwrap_or_else(|e| e.into_inner()) = 0;
        *me.in_portal.lock().unwrap_or_else(|e| e.into_inner()) = false;
        *me.portal_at.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    #[test]
    fn w1_live_wire_explosion_drop_decay_gamerule_gates_spill() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let reg = crate::block_registry::breg().unwrap();
        let _rules = Rules::take(&["tntExplosionDropDecay"]);
        let (me, _peer) = loopback_client("w1boom", 0x7402);
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(me.clone());
        let _clean = LiveCleanup {
            eid: me.eid,
            difficulty: DIFFICULTY.load(Ordering::Relaxed),
        };
        let mut cells = Cells::new();
        cells.set(4, 64, 0, reg.state_of("minecraft:chest", None));
        chests().lock().unwrap_or_else(|e| e.into_inner()).insert(
            ContainerKey::ow((4, 64, 0)),
            HashMap::from([(0i16, (1i64, 3u8, Vec::new()))]),
        );
        _rules.set("tntExplosionDropDecay", 0);
        let rolls0 = W1_EXPLOSION_DROP_ROLLS.load(Ordering::Relaxed);
        primed().lock().unwrap_or_else(|e| e.into_inner()).push(Primed {
            eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
            x: 4.5,
            y: 65.0,
            z: 0.5,
            left: 1,
        });
        game_tick(|name, tick| {
            if matches!(name, "hot_regions" | "tnt") {
                tick();
            }
        });
        assert!(
            W1_EXPLOSION_DROP_ROLLS.load(Ordering::Relaxed) > rolls0,
            "the live TNT phase must consult the explosion-drop-decay gate"
        );
        assert!(
            drops()
                .lock()
                .unwrap()
                .iter()
                .any(|d| d.item == 1 && d.cnt == 3),
            "decay-off chest spill must drop its stack"
        );
        chests().lock().unwrap_or_else(|e| e.into_inner()).remove(&ContainerKey::ow((4, 64, 0)));
    }

    #[test]
    fn w1_live_wire_redstone_observer_and_target_hooks() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let reg = crate::block_registry::breg().unwrap();
        let (me, _peer) = loopback_client("w1rs", 0x7501);
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(me.clone());
        let _clean = LiveCleanup {
            eid: me.eid,
            difficulty: DIFFICULTY.load(Ordering::Relaxed),
        };
        let mut cells = Cells::new();

        // An observer facing east watches the cell at +x; a live `set_block`
        // there must reach `rs_notify_block_update` and schedule its pulse.
        let observer = reg.with_prop(
            reg.state_of("minecraft:observer", None),
            "facing",
            "east",
        );
        cells.set(8, 65, 0, observer);
        cells.record(9, 65, 0);
        set_block(9, 65, 0, reg.state_of("minecraft:stone", None));
        let mut pulsed = false;
        for _ in 0..8 {
            game_tick(|name, tick| {
                if name == "redstone" {
                    tick();
                }
            });
            pulsed |= reg
                .get_prop(block_at(8, 65, 0), "powered")
                .is_some_and(|p| p == "true");
        }
        assert!(pulsed, "observer must pulse through the live redstone pass");

        // A live arrow sticking into a `target` block runs
        // `rs_target_projectile_hit` and powers the block.
        cells.set(4, 64, 4, reg.state_of("minecraft:target", None));
        arrows().lock().unwrap_or_else(|e| e.into_inner()).push(Arrow {
            eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
            owner: -1,
            x: 4.5,
            y: 66.5,
            z: 4.5,
            dx: 0.0,
            dy: -2.0,
            dz: 0.0,
            ttl: 100,
            dmg: 1.0,
            fire: 0,
            punch: 0,
            kind: 0,
            loy: 0,
            ret_item: 0,
            ret_comps: Vec::new(),
            pierce: 0,
            last_hit: -1,
            stuck: false,
        });
        for _ in 0..3 {
            game_tick(|name, tick| {
                if matches!(name, "hot_regions" | "arrows") {
                    tick();
                }
            });
        }
        let power = reg
            .get_prop(block_at(4, 64, 4), "power")
            .and_then(|p| p.parse::<i32>().ok())
            .unwrap_or(0);
        assert!(power > 0, "target block must carry projectile-hit power");
    }
}

#[cfg(test)]
mod wire_ops_fair_play_tests {
    use super::*;

    const FAIR_PLAY_ENV: [&str; 3] = [
        "MC_SPINE_FAIR_PLAY",
        "MC_SPINE_ALLOW_CAVE_MAP",
        "MC_SPINE_ALLOW_ENTITY_RADAR",
    ];

    fn save_env() -> Vec<(&'static str, Option<std::ffi::OsString>)> {
        FAIR_PLAY_ENV
            .iter()
            .map(|name| (*name, std::env::var_os(name)))
            .collect()
    }

    fn restore_env(saved: Vec<(&'static str, Option<std::ffi::OsString>)>) {
        for (name, value) in saved {
            match value {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
    }

    /// `MC_SPINE_FAIR_PLAY` unset or explicit-off emits zero control codes;
    /// only the armed flag emits `§f§a§i§r§x§a§e§r§o`, and the allow switches
    /// follow the documented defaults.
    #[test]
    fn wire_ops_fair_play_codes_only_on_opt_in() {
        let saved = save_env();
        for name in FAIR_PLAY_ENV {
            std::env::remove_var(name);
        }
        // Default (unset): no fair-play, no codes, both maps allowed.
        let policy = map_fair_play_policy();
        assert!(!policy.fair_play);
        assert!(policy.allow_cave_map);
        assert!(policy.allow_entity_radar);
        assert!(map_fair_play_join_chats(&policy).is_empty());

        // Explicit-off spellings must not emit codes either.
        for v in ["0", "false", "off"] {
            std::env::set_var("MC_SPINE_FAIR_PLAY", v);
            assert!(map_fair_play_join_chats(&map_fair_play_policy()).is_empty());
        }

        // Armed: exactly one chat carrying the control code, allow defaults
        // flip to off, an explicit allow still wins.
        std::env::set_var("MC_SPINE_FAIR_PLAY", "1");
        let chats = map_fair_play_join_chats(&map_fair_play_policy());
        assert_eq!(chats.len(), 1);
        let code = "§f§a§i§r§x§a§e§r§o".as_bytes();
        assert!(chats[0].windows(code.len()).any(|w| w == code));
        let policy = map_fair_play_policy();
        assert!(!policy.allow_cave_map);
        assert!(!policy.allow_entity_radar);

        std::env::set_var("MC_SPINE_ALLOW_CAVE_MAP", "true");
        assert!(map_fair_play_policy().allow_cave_map);
        assert!(!map_fair_play_policy().allow_entity_radar);

        restore_env(saved);
    }
}

// ---------------- Lane P8a: parity tests ----------------

#[cfg(test)]
mod p8a_parity_protocol_tests {
    use super::*;
    use std::net::TcpListener;

    /// Restores `world()` overlay cells in a given dimension on drop.
    struct DimCells(Vec<(DimId, i32, i32, i32, Option<u32>)>);
    impl DimCells {
        fn new() -> Self {
            Self(Vec::new())
        }
        fn set(&mut self, dim: DimId, x: i32, y: i32, z: i32, st: u32) {
            let key = (dim, x >> 4, z >> 4);
            let cell = local_idx(x, y, z);
            let prev = {
                let mut w = world().lock().unwrap_or_else(|e| e.into_inner());
                let chunk = w.entry(key).or_default();
                let prev = chunk.get(&cell).copied();
                chunk.insert(cell, st);
                prev
            };
            self.0.push((dim, x, y, z, prev));
        }
    }
    impl std::ops::Drop for DimCells {
        fn drop(&mut self) {
            let mut w = world().lock().unwrap_or_else(|e| e.into_inner());
            for (dim, x, y, z, prev) in self.0.drain(..).rev() {
                if let Some(chunk) = w.get_mut(&(dim, x >> 4, z >> 4)) {
                    match prev {
                        Some(st) => {
                            chunk.insert(local_idx(x, y, z), st);
                        }
                        None => {
                            chunk.remove(&local_idx(x, y, z));
                        }
                    }
                }
            }
        }
    }

    /// A loopback client for opener-count tests: `me.sock` is the end the
    /// server writes to; the returned peer reads exactly those bytes.
    fn p8a_loopback_client(name: &str, eid: i32) -> (Arc<Client>, std::net::TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = std::net::TcpStream::connect(addr).unwrap();
        let (peer, _) = listener.accept().unwrap();
        let me = Arc::new(Client {
            eid,
            uuid: [0x8au8; 16],
            name: name.to_string(),
            player_attack_damage_id: None,
            pos: Mutex::new((0.5, 65.0, 0.5, 0.0, 0.0, true)),
            teleport: Mutex::new(Default::default()),
            hp: Mutex::new(20.0),
            sock: Mutex::new(stream),
            inv: Mutex::new(HashMap::new()),
            cursor: Mutex::new((0, 0, Vec::new())),
            drag: Mutex::new((0, Vec::new())),
            open_chest: Mutex::new(None),
            chest_sync: Mutex::new(None),
            open_furnace: Mutex::new(None),
            craft: Mutex::new(HashMap::new()),
            craft9: Mutex::new(HashMap::new()),
            open_table: Mutex::new(false),
            open_merchant: Mutex::new(false),
            trade_sel: Mutex::new(0),
            merchant_in: Mutex::new([(0, 0, Vec::new()), (0, 0, Vec::new())]),
            bow_draw: Mutex::new(None),
            dig: Mutex::new(None),
            eat_start: Mutex::new(None),
            blocking: Mutex::new(false),
            sprinting: Mutex::new(false),
            sneaking: Mutex::new(false),
            last_melee: Mutex::new(Instant::now() - Duration::from_secs(10)),
            fall_fly: Mutex::new(false),
            invulnerable: Mutex::new(0),
            last_hurt: Mutex::new(0.0),
            shield_cd: Mutex::new(Instant::now() - Duration::from_secs(10)),
            absorb: Mutex::new(0.0),
            pearl_cd: Mutex::new(Instant::now() - Duration::from_secs(10)),
            xp: Mutex::new(0),
            skin: Mutex::new(None),
            seen: Mutex::new(HashSet::new()),
            chunks: Mutex::new(HashSet::from([(-1, -1), (-1, 0), (0, -1), (0, 0)])),
            chunk_ack: Mutex::new(ChunkFeedback::new(1)),
            gen_sources: Mutex::new(GenSources::default()),
            gm: Mutex::new(0),
            fall_from: Mutex::new(65.0),
            food: Mutex::new(20),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
            armor_pts: Mutex::new(0),
            sat: Mutex::new(5.0),
            walk_acc: Mutex::new(0.0),
            lastp: Mutex::new((0.5, 65.0, 0.5)),
            spawn_pt: Mutex::new(None),
            dim: Mutex::new(DimId::Nether),
            spawn_dim: Mutex::new(DimId::Overworld),
            view: Mutex::new(2),
            in_portal: Mutex::new(false),
            portal_at: Mutex::new(None),
            held: Mutex::new(0),
            effs: Mutex::new(Vec::new()),
            lod_v1_enabled: AtomicBool::new(false),
            underground_revealed: AtomicBool::new(false),
            sleeping: Mutex::new(None),
            inv_state_id: AtomicI32::new(0),
        });
        (me, peer)
    }

    /// #87: the right-click chest path resolves the window's keys, its
    /// `isBlockedChestByBlock` gate and its opener counts in the clicking
    /// player's dimension — never the Overworld.
    #[test]
    fn p8a_parity_chest_interact_uses_player_dimension() {
        crate::test_support::require_vanilla_data();
        let Some(reg) = breg() else { return };
        let _guard = world_guard();
        let dim = DimId::Nether;
        let (x, y, z) = (2_500_005, 200, 2_500_007);
        let chest = reg.state_of("minecraft:chest", None);
        let conductor = reg.state_of("minecraft:stone", None);
        let glass = reg.state_of("minecraft:glass", None);
        if reg.name_of(chest) != "minecraft:chest"
            || reg.name_of(conductor) != "minecraft:stone"
            || reg.name_of(glass) != "minecraft:glass"
        {
            return;
        }
        let mut cells = DimCells::new();
        cells.set(dim, x, y, z, chest);
        // A conductor above the Nether chest, a non-conductor at the same cell
        // in the Overworld: the gate must read the Nether cell only.
        cells.set(dim, x, y + 1, z, conductor);
        cells.set(DimId::Overworld, x, y + 1, z, glass);

        let window = chest_window_dim(dim, x, y, z);
        assert_eq!(
            window,
            (ContainerKey::Block((dim, x, y, z)), None),
            "the window key must carry the player's dimension"
        );
        assert!(
            is_blocked_chest_by_block_dim(dim, (x, y, z)),
            "a conductor above the Nether chest blocks it"
        );
        assert!(
            !is_blocked_chest_by_block_dim(DimId::Overworld, (x, y, z)),
            "the Overworld cell holds glass, not a conductor"
        );
        assert!(chest_window_blocked_dim(dim, window));
        assert!(!chest_window_blocked_dim(DimId::Overworld, window));
    }

    /// #87: `ContainerOpenersCounter` counts per dimension — a Nether chest
    /// window counts as an opener of the Nether half only, never the
    /// Overworld cell at the same coordinates.
    #[test]
    fn p8a_parity_chest_opener_count_is_dimension_scoped() {
        let _guard = world_guard();
        let (x, y, z) = (2_500_009, 201, 2_500_011);
        let (client, _peer) = p8a_loopback_client("P8aOpener", -770001);
        *client.open_chest.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((ContainerKey::Block((DimId::Nether, x, y, z)), None));
        let audience = vec![Arc::clone(&client)];
        assert_eq!(half_openers_dim(DimId::Nether, &audience, (x, y, z)), 1);
        assert_eq!(half_openers_dim(DimId::Overworld, &audience, (x, y, z)), 0);
        // The snapshot carries each half's dimension through to the sync.
        let halves = vec![(DimId::Nether, x, y, z)];
        let before = chest_opener_snapshot_dim(&audience, &halves);
        assert_eq!(before, vec![((DimId::Nether, x, y, z), 1)]);
    }

    /// #109: a required resource pack disconnects on Declined, FailedDownload,
    /// InvalidUrl and FailedReload — and only on those, and only when required.
    #[test]
    fn p8a_parity_resource_pack_statuses_disconnect_only_when_required() {
        let _guard = world_guard();
        let saved_pack = std::env::var("MC_SPINE_RESOURCE_PACK_REQUIRED").ok();
        let saved_config = std::env::var_os("MC_SPINE_CONFIG");
        // Point the config lookup at a path that cannot exist so only the env
        // variable drives `resource_pack_required` here.
        std::env::set_var(
            "MC_SPINE_CONFIG",
            std::env::temp_dir().join("mc-spine-p8a-no-such-config.toml"),
        );
        std::env::set_var("MC_SPINE_RESOURCE_PACK_REQUIRED", "1");
        for status in [1i64, 2, 5, 6] {
            assert!(
                resource_pack_status_disconnects(Some(status)),
                "required pack must disconnect on status {status}"
            );
        }
        for status in [0i64, 3, 4, 7] {
            assert!(
                !resource_pack_status_disconnects(Some(status)),
                "status {status} is not a refusal"
            );
        }
        std::env::set_var("MC_SPINE_RESOURCE_PACK_REQUIRED", "0");
        for status in [1i64, 2, 5, 6] {
            assert!(
                !resource_pack_status_disconnects(Some(status)),
                "optional pack never disconnects on status {status}"
            );
        }
        std::env::remove_var("MC_SPINE_RESOURCE_PACK_REQUIRED");
        assert!(!resource_pack_status_disconnects(Some(1)));
        match saved_pack {
            Some(v) => std::env::set_var("MC_SPINE_RESOURCE_PACK_REQUIRED", v),
            None => std::env::remove_var("MC_SPINE_RESOURCE_PACK_REQUIRED"),
        }
        match saved_config {
            Some(v) => std::env::set_var("MC_SPINE_CONFIG", v),
            None => std::env::remove_var("MC_SPINE_CONFIG"),
        }
    }
}

/// Lane BOT-NET-2: the live wiring behind the primitives — the broadcast
/// loops share one `SharedBroadcastFrame` per fanout instead of re-framing
/// and re-compressing per recipient, `game_tick` can run the issue-#105
/// region sweep on an opt-in interval, and standing tamed pets run
/// `pet_follow_teleport_step` inside the live mob tick.
#[cfg(test)]
mod bot_net2_tests {
    use super::*;
    use std::io::Read;
    use std::time::Duration;

    fn bot_client(eid: i32) -> (Arc<Client>, TcpStream) {
        let (mut c, r) = crate::effect_tests::client_pair();
        c.eid = eid;
        r.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        (Arc::new(c), r)
    }

    /// Arm the negotiated compression threshold on a test client's socket the
    /// way `negotiate_compression` does for a live connection.
    fn arm_compression(client: &Client, threshold: usize) {
        let key = {
            let sock = client.sock.lock().unwrap_or_else(|e| e.into_inner());
            socket_key(&sock)
        };
        socket_wire()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key)
            .or_default()
            .compression = Some(threshold);
    }

    fn clear_socket_wire(client: &Client) {
        let key = {
            let sock = client.sock.lock().unwrap_or_else(|e| e.into_inner());
            socket_key(&sock)
        };
        unregister_socket_wire(key);
    }

    /// Everything that arrived on the receiver half of the socket pair.
    fn drained(r: &mut TcpStream) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        out
    }

    /// A compressible ~4 KiB broadcast payload, identical to the shape the
    /// 50-player fanout in BOT-NET-1 measured.
    fn broadcast_payload() -> Vec<u8> {
        let mut payload = Vec::with_capacity(4 * 1024);
        for i in 0..2048u32 {
            payload.extend_from_slice(&[b'a' + (i % 26) as u8, (i >> 8) as u8]);
        }
        payload
    }

    #[test]
    fn bot_net2_broadcast_compresses_once_and_fans_out_byte_identical_frames() {
        let payload = broadcast_payload();
        let (a, mut ra) = bot_client(1);
        let (b, mut rb) = bot_client(2);
        let (plain, mut rp) = bot_client(3);
        arm_compression(&a, 256);
        arm_compression(&b, 256);
        let audience = [a.clone(), b.clone(), plain.clone()];

        let mut shared = None;
        fanout_shared_in(&audience, |_| true, |_c| (0x27, payload.clone()), &mut shared);
        let cache = shared.expect("a delivered broadcast builds the cache");
        assert_eq!(
            cache.frame.encode_count(),
            2,
            "one compressed build for every compressed client plus one plain build"
        );

        let mut body = Vec::new();
        write_varint(&mut body, 0x27);
        body.extend_from_slice(&payload);
        let expected_compressed = connection_limits::encode_compressed_frame(&body, 256);
        let mut expected_plain = Vec::new();
        write_varint(&mut expected_plain, body.len() as i64);
        expected_plain.extend_from_slice(&body);

        assert_eq!(drained(&mut ra), expected_compressed, "client A wire form");
        assert_eq!(drained(&mut rb), expected_compressed, "client B wire form");
        assert_eq!(drained(&mut rp), expected_plain, "plain client wire form");

        for c in [&a, &b, &plain] {
            clear_socket_wire(c);
        }
    }

    #[test]
    fn bot_net2_broadcast_to_one_compression_class_encodes_a_single_frame() {
        let payload = broadcast_payload();
        let (a, mut ra) = bot_client(11);
        let (b, mut rb) = bot_client(12);
        let (c, mut rc) = bot_client(13);
        for client in [&a, &b, &c] {
            arm_compression(client, 256);
        }
        // broadcast_in: everyone except the named sender — three recipients.
        let audience = [a.clone(), b.clone(), c.clone()];
        let mut shared = None;
        fanout_shared_in(&audience, |c| c.eid != 11, |_c| (0x6c, payload.clone()), &mut shared);

        assert_eq!(drained(&mut ra).len(), 0, "the excluded sender gets nothing");
        assert_eq!(
            shared.expect("cache").frame.encode_count(),
            1,
            "three recipients, one wire form, one encode"
        );
        let mut body = Vec::new();
        write_varint(&mut body, 0x6c);
        body.extend_from_slice(&payload);
        let expected = connection_limits::encode_compressed_frame(&body, 256);
        assert_eq!(drained(&mut rb), expected);
        assert_eq!(drained(&mut rc), expected);

        for client in [&a, &b, &c] {
            clear_socket_wire(client);
        }
    }

    /// The world/globals the pet-follow and auto-unload fixtures replace, put
    /// back on drop so a failed assert cannot leak state into the next test.
    struct BotNet2World {
        clients: Vec<Arc<Client>>,
        mobs: Vec<Arc<Mob>>,
        states: HashMap<i32, LiveMobState>,
        world: HashMap<DimChunkKey, HashMap<u32, u32>>,
        region: Region,
        forced: HashSet<(i32, i32)>,
        spawn: (f64, f64, f64, f32),
        tick_idx: u64,
        tickets: HashMap<DimChunkKey, u64>,
        evicted: HashMap<
            (DimId, i32, i32),
            std::collections::BTreeMap<(i32, i32), EvictedChunkArchive>,
        >,
        env_unload: Option<String>,
        env_anvil: Option<String>,
    }

    impl BotNet2World {
        fn take() -> Self {
            Self {
                clients: std::mem::take(
                    &mut *clients().lock().unwrap_or_else(|e| e.into_inner()),
                ),
                mobs: std::mem::take(&mut *mobs().lock().unwrap_or_else(|e| e.into_inner())),
                states: std::mem::take(
                    &mut *live_mob_states()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()),
                ),
                world: std::mem::take(&mut *world().lock().unwrap_or_else(|e| e.into_inner())),
                region: std::mem::take(
                    &mut *region().lock().unwrap_or_else(|e| e.into_inner()),
                ),
                forced: std::mem::take(
                    &mut *forceloaded()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()),
                ),
                spawn: *WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner()),
                tick_idx: TICK_IDX.load(Ordering::Relaxed),
                tickets: std::mem::take(
                    &mut *live_chunk_tickets()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()),
                ),
                evicted: std::mem::take(
                    &mut *evicted_regions()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()),
                ),
                env_unload: std::env::var("MC_SPINE_AUTO_UNLOAD_TICKS").ok(),
                env_anvil: std::env::var("MC_SPINE_ANVIL_WORLD_DIR").ok(),
            }
        }
    }

    impl std::ops::Drop for BotNet2World {
        fn drop(&mut self) {
            *clients().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.clients);
            *mobs().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.mobs);
            *live_mob_states()
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.states);
            *world().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.world);
            *region().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.region);
            *forceloaded()
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.forced);
            *WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner()) = self.spawn;
            TICK_IDX.store(self.tick_idx, Ordering::Relaxed);
            *live_chunk_tickets()
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.tickets);
            *evicted_regions()
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.evicted);
            match self.env_unload.take() {
                Some(v) => std::env::set_var("MC_SPINE_AUTO_UNLOAD_TICKS", v),
                None => std::env::remove_var("MC_SPINE_AUTO_UNLOAD_TICKS"),
            }
            match self.env_anvil.take() {
                Some(v) => std::env::set_var("MC_SPINE_ANVIL_WORLD_DIR", v),
                None => std::env::remove_var("MC_SPINE_ANVIL_WORLD_DIR"),
            }
        }
    }

    /// A wolf mob at `pos` whose live behavior state is a pet owned by
    /// `owner`, `sitting` flag applied, inserted into the live mob set.
    fn tamed_wolf(
        pos: (f64, f64, f64),
        owner: [u8; 16],
        sitting: bool,
    ) -> Arc<Mob> {
        assert!(insert_live_mob(
            122,
            DimId::Overworld,
            (pos.0, pos.1, pos.2, 0.0, 0.0),
            40.0,
            None
        ));
        let wolf = mobs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last()
            .expect("inserted")
            .clone();
        {
            let mut states = live_mob_states()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let state = states.get_mut(&wolf.eid).expect("wolf behavior state");
            let Some(MobBehaviorState::Wolf(pet)) = &mut state.behavior else {
                panic!("insert_live_mob(122) must produce a wolf pet");
            };
            pet.owner = Some(owner);
            pet.sitting = sitting;
        }
        wolf
    }

    /// Stone floor at y=63 with air above across the columns the follow scan
    /// visits, so the teleport lands deterministically at (-2.5, 64.0, -2.5)
    /// for an owner at (0.5, 64.0, 0.5).
    fn lay_pet_floor(stone: u32) {
        for x in -6..=6 {
            for z in -6..=6 {
                set_block(x, 63, z, stone);
                for y in 64..=67 {
                    set_block(x, y, z, 0);
                }
            }
        }
    }

    #[test]
    fn bot_net2_live_tick_teleports_standing_wolf_past_twelve_blocks() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        let _guard = world_guard();
        let _saved = BotNet2World::take();
        let stone = name_to_state("stone").expect("stone must exist");
        lay_pet_floor(stone);

        let (mut owner_c, _rx) = crate::effect_tests::client_pair();
        owner_c.uuid = [7u8; 16];
        *owner_c.pos.lock().unwrap_or_else(|e| e.into_inner()) =
            (0.5, 64.0, 0.5, 0.0, 0.0, true);
        *clients().lock().unwrap_or_else(|e| e.into_inner()) = vec![Arc::new(owner_c)];

        let wolf = tamed_wolf((13.5, 64.0, 0.5), [7u8; 16], false);
        refresh_hot();
        mob_tick();

        let (x, y, z, ..) = *wolf.pos.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            (x, y, z),
            (-2.5, 64.0, -2.5),
            "a standing wolf 13 blocks out reseats on the safe floor next to its owner"
        );
    }

    #[test]
    fn bot_net2_live_tick_keeps_sitting_wolf_put() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        let _guard = world_guard();
        let _saved = BotNet2World::take();
        let stone = name_to_state("stone").expect("stone must exist");
        lay_pet_floor(stone);
        // Floor under the sitting wolf too, so nothing but the pet rule can
        // move it.
        for x in -14..=-12 {
            for z in -2..=2 {
                set_block(x, 63, z, stone);
                set_block(x, 64, z, 0);
                set_block(x, 65, z, 0);
            }
        }

        let (mut owner_c, _rx) = crate::effect_tests::client_pair();
        owner_c.uuid = [9u8; 16];
        *owner_c.pos.lock().unwrap_or_else(|e| e.into_inner()) =
            (0.5, 64.0, 0.5, 0.0, 0.0, true);
        *clients().lock().unwrap_or_else(|e| e.into_inner()) = vec![Arc::new(owner_c)];

        let wolf = tamed_wolf((-13.5, 64.0, 0.5), [9u8; 16], true);
        refresh_hot();
        mob_tick();

        let (x, y, z, ..) = *wolf.pos.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            (x, y, z),
            (-13.5, 64.0, 0.5),
            "a sitting wolf stays exactly where it was told to sit"
        );
    }

    #[test]
    fn bot_net2_auto_unload_respects_env_and_parks_evicted_region() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        let _guard = world_guard();
        let _saved = BotNet2World::take();
        // The default is off: the stage must not touch resident state no
        // matter where TICK_IDX sits — this is what keeps the saved-payload
        // digest bit-identical with the hook wired in.
        std::env::remove_var("MC_SPINE_AUTO_UNLOAD_TICKS");
        std::env::remove_var("MC_SPINE_ANVIL_WORLD_DIR");
        *WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner()) = (0.5, 64.0, 0.5, 0.0);
        assert_eq!(auto_unload_period(), 0, "unset disables the stage");
        std::env::set_var("MC_SPINE_AUTO_UNLOAD_TICKS", "0");
        assert_eq!(auto_unload_period(), 0, "an explicit 0 disables too");
        std::env::set_var("MC_SPINE_AUTO_UNLOAD_TICKS", "abc");
        assert_eq!(auto_unload_period(), 0, "unparseable disables");
        std::env::set_var("MC_SPINE_AUTO_UNLOAD_TICKS", "600");
        assert_eq!(auto_unload_period(), 600);

        let stone = name_to_state("stone").expect("stone must exist");
        set_block_dim(DimId::Overworld, 800, 64, 800, stone); // chunk (50, 50)
        let overlay_len = || {
            world()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&(DimId::Overworld, 50, 50))
                .map_or(0, |chunk| chunk.len())
        };

        // Disabled at a tick boundary that would fire if enabled: nothing moves.
        std::env::remove_var("MC_SPINE_AUTO_UNLOAD_TICKS");
        TICK_IDX.store(600, Ordering::Relaxed);
        auto_unload_tick();
        assert_eq!(overlay_len(), 1, "disabled stage must not evict");

        // Enabled at period 5: off-boundary ticks skip, on-boundary ticks run.
        std::env::set_var("MC_SPINE_AUTO_UNLOAD_TICKS", "5");
        TICK_IDX.store(602, Ordering::Relaxed);
        auto_unload_tick();
        assert_eq!(overlay_len(), 1, "602 is not a multiple of 5");
        TICK_IDX.store(605, Ordering::Relaxed);
        auto_unload_tick();
        assert_eq!(
            overlay_len(),
            0,
            "605 fires the sweep; the unticketed far chunk evicts"
        );
        assert!(
            !evicted_regions()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "with no Anvil dir the unload parks in evicted_regions"
        );

        // Re-hydration brings the edit back byte-for-byte.
        assert!(rehydrate_evicted_chunk(DimId::Overworld, 50, 50));
        assert_eq!(
            block_at_dim(DimId::Overworld, 800, 64, 800),
            stone,
            "the parked chunk restores its block"
        );
    }
}
