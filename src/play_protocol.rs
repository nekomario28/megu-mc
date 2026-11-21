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
        let (id, payload) = decode_packet(body, self.compression, ingress_guard::MAX_PLAY_FRAME)?;
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

fn negotiate_compression(
    s: &mut TcpStream,
    threshold: i32,
) -> std::io::Result<Option<LoginCompression>> {
    let Ok(size) = usize::try_from(threshold) else {
        return Ok(None);
    };
    send_packet(s, 0x03, connection_limits::set_compression(threshold))?;
    let key = socket_key(s);
    socket_wire().lock().unwrap().insert(
        key,
        SocketWireState {
            compression: Some(size),
            play: None,
        },
    );
    Ok(Some(LoginCompression(key)))
}

fn send_packet(s: &mut TcpStream, mut id: i64, mut payload: Vec<u8>) -> std::io::Result<()> {
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
    let frame = match compression {
        Some(threshold) => connection_limits::encode_compressed_frame(body, threshold),
        None => {
            let mut frame = Vec::new();
            write_varint(&mut frame, body.len() as i64);
            frame.extend_from_slice(body);
            frame
        }
    };
    let result = s.write_all(&frame);
    if result.is_err() {
        // A partial write invalidates both the frame boundary and entity baselines.
        let _ = s.shutdown(std::net::Shutdown::Both);
    }
    result
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
}

impl Client {
    fn enter_play_wire(
        &self,
        compression: Option<usize>,
        chunk_delivery: connection_limits::ChunkDelivery,
    ) {
        let key = socket_key(&self.sock.lock().unwrap());
        socket_wire().lock().unwrap().insert(
            key,
            SocketWireState {
                compression,
                play: Some(PlayWireState {
                    chunk_delivery,
                    ..PlayWireState::default()
                }),
            },
        );
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
    for c in audience.iter() {
        if !interested(c) {
            continue;
        }
        let pl = mk(c);
        if let Ok(mut w) = c.sock.lock() {
            let _ = send_packet(&mut w, pl.0, pl.1);
        }
    }
}

fn broadcast<F: Fn(&Client) -> (i64, Vec<u8>)>(me_eid: i32, mk: F) {
    broadcast_in(&client_snapshot(), me_eid, mk)
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
    for m in mobs().lock().unwrap().iter() {
        let p = *m.pos.lock().unwrap();
        let m = m.clone();
        queue(
            m.dim,
            m.etype,
            (
                m.eid,
                p.0,
                p.1,
                p.2,
                Box::new(move || live_mob_spawn_packets(&m)),
            ),
        );
    }
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
                Box::new(move || vec![(0x01, spawn_player(&o2))]),
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
        crops().lock().unwrap().remove(&ow_block((x, y, z)));
        if old == 4285 {
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
        if let Some(item) = if !can_harvest(old as i64, held.0) {
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
        if send_packet(&mut socket, id, packet).is_err() {
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
    for client in audience.iter().filter(|c| c.eid != excl) {
        // Serialize updates with the tracking sweep's spawn and remove packets.
        let seen = client.seen.lock().unwrap();
        if seen.contains(&eid) {
            let (id, packet) = mk(client);
            if let Ok(mut socket) = client.sock.lock() {
                let _ = send_packet(&mut socket, id, packet);
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
            let _ = send_packet(&mut socket, id, packet);
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
fn resource_pack_required() -> bool {
    std::env::var("MC_SPINE_RESOURCE_PACK_REQUIRED")
        .is_ok_and(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
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
    config
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
    const MASK_WIRE_SIZE: usize = 2 * (1 + 8) + 2; // two one-long and two empty bitsets
    let block_count = dim.light_sections();
    let sky_count = if worldgen::has_skylight(dim) {
        block_count
    } else {
        0
    };
    let suffix_size = MASK_WIRE_SIZE + 2 + (sky_count + block_count) * ARRAY_WIRE_SIZE;
    let Some(start) = packet.len().checked_sub(suffix_size) else {
        return;
    };
    let mut light = &packet[start..];
    for expected in [
        if sky_count == 0 { 0 } else { dim.light_mask() },
        dim.light_mask(),
    ] {
        if read_protocol_i32(&mut light) != Some(1) || !light.starts_with(&expected.to_be_bytes()) {
            return;
        }
        light = &light[8..];
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
        let present = bands[layer].iter().fold(0u64, |mask, &i| mask | (1 << i));
        (present, ((1 << lists[layer].len()) - 1) ^ present)
    });
    let array_bytes = (bands[0].len() + bands[1].len()) * ARRAY_WIRE_SIZE;
    let mut sparse = Vec::with_capacity(4 * (1 + 8) + 2 + array_bytes);
    for mask in [sky, block, empty_sky, empty_block] {
        write_varint(&mut sparse, i64::from(mask != 0));
        if mask != 0 {
            sparse.extend_from_slice(&mask.to_be_bytes());
        }
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
fn cached_chunk_frame(
    compression: Option<usize>,
    gen: &mut GenSources,
    dim: DimId,
    cx: i32,
    cz: i32,
) -> Arc<Vec<u8>> {
    let key = (dim, cx, cz);
    let generation = current_gen_seq();
    // `live_chunk_block_entities` iterates a map; sort so the spliced bytes
    // (and their fingerprint) are deterministic across readers.
    let mut entities = live_chunk_block_entities(dim, cx, cz);
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
    let mut packet = map_chunk(cx, cz, dim, gen);
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
            let frame = cached_chunk_frame(compression, gen, dim, *cx, *cz);
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
        let pkt = update_light_packet_dim(dim, cx, cz, gen);
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

fn pump_client_chunks(client: &Client, announce_center: bool) -> bool {
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
        let mut ids = [-1i32; 3];
        for (index, (entry, _)) in registry_wire_entries(body)?.into_iter().enumerate() {
            if let Some(dim) = DimId::parse_dimension_id(entry) {
                ids[dim.index()] = index as i32;
            }
        }
        return ids.iter().all(|&id| id >= 0).then_some(ids);
    }
    None
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
    dim_type_wire_ids()[dim.index()]
}

/// 0x2b Join Game. Every dimension type the client can be moved into is declared so a
/// later 0x47 Respawn can name one the client already knows; the spawn info carries the
/// dimension the player actually starts in.
fn join_game(dim: DimId, gm: u8, eid: i32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&eid.to_be_bytes());
    p.push(0);
    write_varint(&mut p, 3);
    for d in DimId::ALL {
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
    if *me.gm.lock().unwrap() != 0 {
        enter_portal(me, st, bx, by, bz, yaw, pitch, view);
        return true;
    }
    // Survival: vanilla's 4-second portal delay. Creative/spectator is instant.
    let mut at = me.portal_at.lock().unwrap();
    if at.is_none() {
        *at = Some(Instant::now());
        return false;
    }
    if at.unwrap().elapsed() < Duration::from_secs(4) {
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

fn handle(mut s: TcpStream) {
    s.set_nodelay(true).ok();
    let peer = s
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
            let online = clients().lock().unwrap().len();
            let max = std::env::var("MC_SPINE_MAX_PLAYERS").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(20);
            let motd = std::env::var("MC_SPINE_MOTD").unwrap_or_else(|_| "mc-spine".to_owned());
            let motd = motd.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n").replace('\r', "\\r");
            let json = format!("{{\"version\":{{\"name\":\"1.21.1\",\"protocol\":767}},\"players\":{{\"max\":{max},\"online\":{online}}},\"description\":{{\"text\":\"{motd}\"}}}}");
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
    let Some((name, uuid)) = login_identity(&lbody) else {
        return;
    };
    if let Some(list) = connection_limits::allowlist() {
        if !list.permits(&name, &uuid) {
            eprintln!("[security] login refused for {name} from {peer}: not on the allowlist");
            login_disconnect(&mut s, "You are not white-listed on this server!");
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
    // Keep the existing uncompressed default. Zero compresses every packet.
    let threshold = std::env::var("MC_SPINE_COMPRESSION_THRESHOLD")
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .unwrap_or(-1);
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
    let login_admin = player_is_admin(&name);
    if send_commands(&mut s, login_admin).is_err() {
        return;
    }
    let _ = send_packet(&mut s, 0x53, vec![initial_held as u8]);
    let mut vp = Vec::new();
    write_varint(&mut vp, 0);
    write_varint(&mut vp, 0);
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
    // before Join Game, and is threaded through the first stream as well.
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
    if let Some(rp) = resource_pack_pkt() {
        let _ = send_packet(&mut s, 0x46, rp);
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
        skin: Mutex::new(None),
        held: Mutex::new(initial_held),
        effs: Mutex::new(saved_player.as_ref().map_or_else(Vec::new, |save| {
            save.effects.iter().map(|&(id, amp, left)| Eff { id, amp, left }).collect()
        })),
    });
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
    kick_skin(name.clone(), &me);
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
    // ---- consume loop ----
    s.set_write_timeout(Some(Duration::from_secs(2))).ok();
    let mut keepalive = connection_limits::KeepaliveState::new(Instant::now());
    let mut next_keepalive_id = 1i64;
    let mut held_slot: i16 = initial_held;

    let mut rx = RxBuf::new(compression);
    // Play-state packet inactivity is tracked separately from the keep-alive deadline.
    let mut last_rx = Instant::now();
    let mut chat_throttle = connection_limits::ChatThrottle::new(Instant::now());
    let mut read_timeout = None;
    let mut last_chunk_retry_tick = TICK_IDX.load(Ordering::Relaxed);
    'conn: loop {
        drop_player_items_on_death(&me);
        use connection_limits::KeepaliveAction;
        match keepalive.tick(Instant::now(), next_keepalive_id) {
            KeepaliveAction::Send(id) => {
                if send_packet(&mut me.sock.lock().unwrap(), 0x26, id.to_be_bytes().to_vec())
                    .is_err()
                {
                    break 'conn;
                }
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
                    let teleport = me.teleport.lock().unwrap();
                    if !teleport.movement_allowed() {
                        continue;
                    }
                    let mut p = me.pos.lock().unwrap();
                    let (mut yaw, mut pitch, mut gnd) = (p.3, p.4, p.5);
                    if pid == 0x1b && b.len() >= 33 {
                        yaw = f32::from_be_bytes(b[24..28].try_into().unwrap());
                        pitch = f32::from_be_bytes(b[28..32].try_into().unwrap());
                        gnd = b[32] != 0;
                    } else if pid == 0x1a && b.len() >= 25 {
                        gnd = b[24] != 0;
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
                    let teleport = me.teleport.lock().unwrap();
                    if !teleport.movement_allowed() {
                        continue;
                    }
                    let mut p = me.pos.lock().unwrap();
                    let prev_gnd = p.5;
                    p.3 = yaw;
                    p.4 = pitch;
                    p.5 = b[8] != 0;
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
                // held_item_slot: i16
                if b.len() >= 2 {
                    held_slot = i16::from_be_bytes(b[..2].try_into().unwrap()).clamp(0, 8);
                    *me.held.lock().unwrap() = held_slot;
                }
            }
            0x1d => {
                // flying: onGround only
                if !b.is_empty() {
                    let teleport = me.teleport.lock().unwrap();
                    if !teleport.movement_allowed() {
                        continue;
                    }
                    let mut p = me.pos.lock().unwrap();
                    let prev_gnd = p.5;
                    p.5 = b[0] != 0;
                    drop(p);
                    drop(teleport);
                    landing::player_move(&me, prev_gnd);
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
                    if in_range && old != 0 && game_mode != 2 {
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
                    let window = chest_window(x, y, z);
                    // `ChestBlock.getMenuProvider` (`dhb.b`): refuses single or double
                    // chest if either half has a redstone conductor or sitting cat above.
                    // `BarrelBlock` (`dfe.a`) does not check `isBlockedChestByBlock`.
                    if (kind == "Chest" && chest_window_blocked(window))
                        || (me.is_spectator() && window_has_pending_loot(window))
                    {
                        let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                        continue;
                    }
                    let audience = client_snapshot();
                    let mut halves = chest_window_halves(window);
                    // A second container opening on the same client closes the first,
                    // which drops the first one's opener count too.
                    if let Some((previous, prev_anchor)) = client_open_window_and_anchor(&me) {
                        for half in client_window_halves(previous, prev_anchor) {
                            if !halves.contains(&half) {
                                halves.push(half);
                            }
                        }
                    }
                    let before = chest_opener_snapshot(&audience, &halves);
                    close_live_menu(&me, false, true);
                    let inv = me.inv.lock().unwrap().clone();
                    open_chest_for_client(&me, window, None, &inv, kind);
                    // `ContainerOpenersCounter.incrementOpeners` -> openerCountChanged
                    sync_chest_openers(&audience, &before);
                    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                    continue;
                }
                // ender chest: per-player shared storage (synthetic key, not world pos)
                if (7513..=7520).contains(&block_at_dim(dim, x, y, z)) {
                    // `EnderChestBlock.useWithoutItem` (`dip.a`): refuses when the cell
                    // above `isRedstoneConductor`.
                    if is_blocked_chest_by_block((x, y, z)) {
                        let _ = send_packet(&mut me.sock.lock().unwrap(), 0x05, ack(seq));
                        continue;
                    }
                    let audience = client_snapshot();
                    let mut halves = vec![(x, y, z)];
                    if let Some((previous, prev_anchor)) = client_open_window_and_anchor(&me) {
                        for half in client_window_halves(previous, prev_anchor) {
                            if !halves.contains(&half) {
                                halves.push(half);
                            }
                        }
                    }
                    let before = chest_opener_snapshot(&audience, &halves);
                    let key = ender_container_key(&me.uuid);
                    close_live_menu(&me, false, true);
                    let inv = me.inv.lock().unwrap().clone();
                    open_chest_for_client(&me, (key, None), Some((x, y, z)), &inv, "Ender Chest");
                    sync_chest_openers(&audience, &before);
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
                    if dim == DimId::Overworld && (13000..=23500).contains(&wt) {
                        WORLD_TIME.store(0, Ordering::Relaxed);
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
                        let line = system_chat(&format!("{} slept through the night", me.name));
                        broadcast(-1, |_c| (0x6c, line.clone()));
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
                    let st = block_at_dim(dim, x, y, z);
                    let iron = breg().is_some_and(|reg| reg.name_of(st) == "minecraft:iron_trapdoor");
                    if !iron {
                        let opened = breg().map(|reg| reg.get_prop(st, "open").as_deref() == Some("true"));
                        if let (Some(reg), Some(opened)) = (breg(), opened) {
                            let updated = reg.with_prop(st, "open", if opened { "false" } else { "true" });
                            set_block_dim(dim, x, y, z, updated);
                            note_block_update_dim(dim, x, y, z, updated);
                        }
                    }
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
                        crops().lock().unwrap().insert(ow_block((x, y, z)), nage);
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
                        crops().lock().unwrap().insert(ow_block((tx, ty, tz)), 0);
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
                                chest_placement_state(
                                    name,
                                    (tx, ty, tz),
                                    yaw,
                                    sneaking,
                                    face.clamp(0, 5) as u8,
                                )
                            }
                            None => st,
                        };
                        place((tx, ty, tz), st);
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
                // chat_message: first field = message string
                let mut sl = &b[..];
                if let Some(mlen) = read_varint(&mut sl) {
                    let m =
                        String::from_utf8_lossy(&sl[..(mlen as usize).min(sl.len())]).to_string();
                    // vanilla: over-long or illegal text disconnects, and so does spam
                    if !connection_limits::chat_text_valid(&m) {
                        eprintln!("[spine] {} disconnected: illegal chat message", me.name);
                        play_disconnect(&mut me.sock.lock().unwrap(), "Illegal characters in chat");
                        break 'conn;
                    }
                    if !chat_throttle.allow(Instant::now()) {
                        eprintln!("[spine] {} disconnected: chat spam", me.name);
                        play_disconnect(&mut me.sock.lock().unwrap(), "Kicked for spamming!");
                        break 'conn;
                    }
                    if m.starts_with('/') {
                        run_command(Some(&me), &m, &mut Some(&me));
                        continue;
                    }
                    #[cfg(feature = "neoforge-events")]
                    let m = if java_compat::NEOFORGE.get().is_some() {
                        match neoforge_event(&me, java_compat::S1Event::ServerChat {
                            player: (me.eid, me.name.clone()),
                            raw_text: m.clone(),
                        }) {
                            Ok(rewritten) => rewritten.unwrap_or(m),
                            Err(_) => continue,
                        }
                    } else {
                        m
                    };
                    let line = format!("<{}> {}", me.name, m);
                    let pl = system_chat(&line);
                    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x6c, pl.clone());
                    broadcast(me.eid, |_c| (0x6c, pl.clone()));
                }
            }
            #[cfg(feature = "neoforge-events")]
            0x12 => neoforge_payload(&me, &b),
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
                                    let halves = client_window_halves(prev, prev_anchor);
                                    let snap = chest_opener_snapshot(&aud, &halves);
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
                                sync_chest_openers(&audience, &before);
                            }
                        }
                        continue;
                    }
                    if live_mob_interact(&me, target, 36 + held_slot) {
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
                                let line =
                                    system_chat(&format!("{} was slain by {}", vname, me.name));
                                broadcast(-1, |_c| (0x6c, line.clone()));
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
                        let line = system_chat(&format!("{} was slain by {}", mn, me.name));
                        broadcast(-1, |_c| (0x6c, line.clone()));
                        let Some((mob, state)) = remove_live_mob(deid) else { continue };
                        let burn_kill = *mob.fire_ticks.lock().unwrap() > 0;
                        live_mob_death(&mob, state, [mx, my, mz]);
                        broadcast_seen_destroy(deid);
                        let was_sheared = sheared().lock().unwrap().remove(&deid);
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
            0x09 => {
                // client_command: perform_respawn
                let mut sl = &b[..];
                if read_varint(&mut sl) == Some(0) && sl.is_empty() {
                    drop_player_items_on_death(&me);
                    let restored_health = max_hp_of(&me.name);
                    {
                        let mut health = me.hp.lock().unwrap();
                        if *health > 0.0 {
                            continue;
                        }
                        *health = restored_health;
                    }
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
                if resource_pack_required() && matches!(status, Some(1 | 2 | 5 | 6)) {
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
                    let halves = client_window_halves(open, anchor);
                    let before = chest_opener_snapshot(&audience, &halves);
                    *me.open_chest.lock().unwrap() = None;
                    *me.chest_sync.lock().unwrap() = None;
                    sync_chest_openers(&audience, &before);
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
                    if read_protocol_i32(&mut sl).is_none() {
                        continue;
                    }
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
            let halves = client_window_halves(open, anchor);
            let before = chest_opener_snapshot(&audience, &halves);
            *me_cleanup.open_chest.lock().unwrap() = None;
            *me_cleanup.chest_sync.lock().unwrap() = None;
            sync_chest_openers(&audience, &before);
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
        player_mechanism_sessions().lock().unwrap().remove(&me_cleanup.eid);
        death_drop_marks().lock().unwrap().remove(&me_cleanup.eid);
        beacon_menu_anchors().lock().unwrap().remove(&me_cleanup.eid);
        // vanilla culls the destroy to this entity's trackers
        broadcast_seen_destroy(me_cleanup.eid);
        let mut p = Vec::new();
        write_varint(&mut p, 1);
        p.extend_from_slice(&me_cleanup.uuid);
        broadcast(me_cleanup.eid, |_c| (0x3d, p.clone()));
    }
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
        let f1 = cached_chunk_frame(None, &mut gen, dim, cx, cz);
        let mut gen2 = GenSources::new();
        let f2 = cached_chunk_frame(None, &mut gen2, dim, cx, cz);
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
        let f3 = cached_chunk_frame(None, &mut gen, dim, cx, cz);
        assert!(!Arc::ptr_eq(&f1, &f3), "the edit must not hit the old frame");
        assert_ne!(f1, f3, "the rebuild carries the edited cell");
        let f4 = cached_chunk_frame(None, &mut GenSources::new(), dim, cx, cz);
        assert!(Arc::ptr_eq(&f3, &f4), "the rebuilt frame is shared too");
        // Compressed readers share one lazily compressed frame of the same body.
        let c1 = cached_chunk_frame(Some(256), &mut gen, dim, cx, cz);
        let c2 = cached_chunk_frame(Some(256), &mut gen, dim, cx, cz);
        assert!(Arc::ptr_eq(&c1, &c2));
        set_block_dim(dim, 8, 70, 8, cur);
        let f5 = cached_chunk_frame(None, &mut gen, dim, cx, cz);
        assert_eq!(f1, f5, "restoring the cell restores the bytes");
    }
}
