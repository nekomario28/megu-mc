// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
// Opt-in `mc-spine:lod_v1` server plugin channel (Wave B3).
//
// Negotiation rides the standard C2S `0x12` plugin-channel payloads: a client
// that registers `mc-spine:lod_v1` on `minecraft:register` is answered with an
// S2C `0x19` echo and may then send `REQ1` column requests on the channel.
// A vanilla client never registers the channel and therefore never sees a
// `lod_v1` packet. The `LOD1` frame layout is byte-for-byte the codec in
// `client-app/src/lod.rs` (`encode_lod_v1` / `decode_lod_v1`).

/// The plugin channel a LOD client registers to opt in.
const LOD_V1_CHANNEL: &str = "mc-spine:lod_v1";

/// `LOD1` frame constants shared with `client-app/src/lod.rs`.
const LOD_V1_MAGIC: &[u8; 4] = b"LOD1";
const LOD_V1_VERSION: u8 = 1;
const LOD_V1_HEADER: usize = 16;
const LOD_V1_CELL_BYTES: usize = 11;

/// `REQ1` request frame: magic, resolution, `cx`/`cz` i32 little-endian.
const LOD_V1_REQ_MAGIC: &[u8; 4] = b"REQ1";
const LOD_V1_REQ_BYTES: usize = 13;

/// The two supported resolutions, matching `LOD_COARSE`/`LOD_FINE` in
/// `client-port/src/lod.rs`: one cell per 4x4 block footprint, or per column.
const LOD_RES_COARSE: u8 = 4;
const LOD_RES_FINE: u8 = 16;

/// Channel-name and payload caps on the C2S `0x12` body, the same bounds the
/// mod-runtime path applies (`S4_MAX_CHANNEL`/`S4_MAX_PAYLOAD`), mirrored here
/// because that path is feature-gated and this dispatch is not.
const LOD_V1_MAX_CHANNEL: usize = 256;
const LOD_V1_MAX_PAYLOAD: usize = 32 * 1024;

/// A requester may ask only for columns inside this Chebyshev chunk radius of
/// its own chunk: distant requests force worldgen, and each in-range request
/// runs up to 256 heightmap scans. 64 chunks already covers every sane LOD
/// horizon; the original 128 multiplied the worst-case fan-out fourfold.
const LOD_V1_MAX_RANGE_CHUNKS: i32 = 64;

/// One `LOD1` answer costs up to 256 `lod_top_cell` scans (each possibly
/// generating a chunk), so encoded frames are cached per
/// `(dim, cx, cz, resolution)` and capped: the oldest entry is evicted once
/// the cap is reached, and a block write in the chunk invalidates the frame.
const LOD_V1_CACHE_CAP: usize = 2048;

/// Per-connection `REQ1` budget: up to `LOD_V1_REQ_BURST` requests may burst,
/// then the bucket refills at `LOD_V1_REQ_PER_SEC` per second. Requests over
/// budget are dropped the same way malformed ones are — silently, and before
/// any world scan runs.
const LOD_V1_REQ_BURST: u32 = 16;
const LOD_V1_REQ_PER_SEC: u32 = 8;

/// How many column scans ran (cache misses): the counter tests read to prove
/// a cache hit skipped `encode_lod_v1_column`.
static LOD_V1_COLUMN_SCANS: AtomicU64 = AtomicU64::new(0);

/// How far above the reported surface the top scan reaches for trees and other
/// above-ground generated blocks before it gives up.
const LOD_SCAN_ABOVE_SURFACE: i32 = 32;

/// One wire cell of a `LOD1` frame. `top_y` is `i16::MIN` when the footprint
/// holds no non-air block; `light` packs `(sky << 4) | block` nibbles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct LodCellV1 {
    state_id: u32,
    tint: u32,
    top_y: i16,
    light: u8,
}

const LOD_CELL_EMPTY: LodCellV1 = LodCellV1 {
    state_id: 0,
    tint: 0,
    top_y: i16::MIN,
    light: 0,
};

/// A decoded `LOD1` frame, for round-trip verification against the client
/// codec's layout.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LodFrameV1 {
    cx: i32,
    cz: i32,
    resolution: u8,
    cells: Vec<LodCellV1>,
}

/// Encode one column summary as a `LOD1` frame; `cells` must hold exactly
/// `resolution * resolution` entries in x-fastest, z-major order, matching
/// `client-app/src/lod.rs::encode_lod_v1` byte for byte.
fn encode_lod_v1(cx: i32, cz: i32, resolution: u8, cells: &[LodCellV1]) -> Option<Vec<u8>> {
    if !matches!(resolution, LOD_RES_COARSE | LOD_RES_FINE) {
        return None;
    }
    if cells.len() != usize::from(resolution) * usize::from(resolution) {
        return None;
    }
    let mut frame = Vec::with_capacity(LOD_V1_HEADER + cells.len() * LOD_V1_CELL_BYTES);
    frame.extend_from_slice(LOD_V1_MAGIC);
    frame.push(LOD_V1_VERSION);
    frame.push(resolution);
    frame.extend_from_slice(&(cells.len() as u16).to_le_bytes());
    frame.extend_from_slice(&cx.to_le_bytes());
    frame.extend_from_slice(&cz.to_le_bytes());
    for cell in cells {
        frame.extend_from_slice(&cell.state_id.to_le_bytes());
        frame.extend_from_slice(&cell.tint.to_le_bytes());
        frame.extend_from_slice(&cell.top_y.to_le_bytes());
        frame.push(cell.light);
    }
    Some(frame)
}

/// Decode a `LOD1` frame with the same rejections as the client codec:
/// truncated, trailing bytes, wrong magic/version, bad resolution or a cell
/// count that does not match `resolution * resolution`.
#[allow(dead_code)]
fn decode_lod_v1_frame(frame: &[u8]) -> Option<LodFrameV1> {
    if frame.len() < LOD_V1_HEADER || !frame.starts_with(LOD_V1_MAGIC) {
        return None;
    }
    if frame[4] != LOD_V1_VERSION {
        return None;
    }
    let resolution = frame[5];
    if !matches!(resolution, LOD_RES_COARSE | LOD_RES_FINE) {
        return None;
    }
    let count = u16::from_le_bytes([frame[6], frame[7]]) as usize;
    if count != usize::from(resolution) * usize::from(resolution) {
        return None;
    }
    if frame.len() != LOD_V1_HEADER + count * LOD_V1_CELL_BYTES {
        return None;
    }
    let cx = i32::from_le_bytes(frame[8..12].try_into().expect("header cx"));
    let cz = i32::from_le_bytes(frame[12..16].try_into().expect("header cz"));
    let mut cells = Vec::with_capacity(count);
    let mut cursor = LOD_V1_HEADER;
    for _ in 0..count {
        cells.push(LodCellV1 {
            state_id: u32::from_le_bytes(frame[cursor..cursor + 4].try_into().expect("state_id")),
            tint: u32::from_le_bytes(frame[cursor + 4..cursor + 8].try_into().expect("tint")),
            top_y: i16::from_le_bytes(
                frame[cursor + 8..cursor + 10].try_into().expect("top_y"),
            ),
            light: frame[cursor + 10],
        });
        cursor += LOD_V1_CELL_BYTES;
    }
    Some(LodFrameV1 {
        cx,
        cz,
        resolution,
        cells,
    })
}

/// `true` when `state` occupies the column top for LOD purposes: every non-air
/// block counts, fluids included, matching `Block::renderable` on the client.
fn lod_non_air(state: u32) -> bool {
    match breg() {
        Some(reg) => !reg.is_air(state),
        None => state != 0,
    }
}

/// The ARGB tint the client would attach to this top block itself
/// (`client-app/src/world.rs`): water takes the fluid tint, the tinted
/// grass/foliage set takes the plains grass tint, everything else sends 0 so
/// the client falls back to the palette colour.
fn lod_tint(state: u32) -> u32 {
    let Some(reg) = breg() else {
        return 0;
    };
    let short = reg
        .name_of(state)
        .strip_prefix("minecraft:")
        .unwrap_or_else(|| reg.name_of(state));
    if short == "water" {
        return 0x3f76e4;
    }
    if short == "grass_block"
        || short.ends_with("_leaves")
        || short == "short_grass"
        || short == "tall_grass"
        || short == "fern"
        || short == "large_fern"
        || short == "vine"
        || short == "sugar_cane"
    {
        return 0x7cbd6b;
    }
    0
}

/// The highest non-air block in block column `(x, z)` of `dim`, or
/// [`LOD_CELL_EMPTY`]. The scan starts just above the dimension's reported
/// surface (`height_at_dim`) so trees and water overhangs are captured, and it
/// stops at the surface, so buried ores and caves can never reach the wire.
fn lod_top_cell(dim: DimId, x: i32, z: i32) -> LodCellV1 {
    let world_top = dim.min_y().saturating_add(dim.height()).saturating_sub(1);
    let mut y = height_at_dim(dim, x, z)
        .saturating_add(LOD_SCAN_ABOVE_SURFACE)
        .min(world_top);
    while y >= dim.min_y() {
        let state = block_at_dim(dim, x, y, z);
        if lod_non_air(state) {
            return LodCellV1 {
                state_id: state,
                tint: lod_tint(state),
                top_y: y.clamp(i32::from(i16::MIN) + 1, i32::from(i16::MAX)) as i16,
                light: if dim.has_skylight() { 0xf0 } else { 0 },
            };
        }
        y -= 1;
    }
    LOD_CELL_EMPTY
}

/// The `resolution * resolution` cells for chunk column `(cx, cz)` of `dim`.
/// Resolution 16 summarizes each block column; resolution 4 keeps the highest
/// top of each 4x4 footprint, matching the client's `summarize_column`.
fn lod_column_cells(dim: DimId, cx: i32, cz: i32, resolution: u8) -> Option<Vec<LodCellV1>> {
    if !matches!(resolution, LOD_RES_COARSE | LOD_RES_FINE) {
        return None;
    }
    let base_x = cx.checked_mul(16)?;
    let base_z = cz.checked_mul(16)?;
    let mut tops = [LOD_CELL_EMPTY; 256];
    for z in 0..16usize {
        for x in 0..16usize {
            tops[z * 16 + x] = lod_top_cell(dim, base_x + x as i32, base_z + z as i32);
        }
    }
    if resolution == LOD_RES_FINE {
        return Some(tops.to_vec());
    }
    let edge = usize::from(resolution);
    let foot = 16 / edge;
    let mut out = vec![LOD_CELL_EMPTY; edge * edge];
    for cz2 in 0..edge {
        for cx2 in 0..edge {
            let mut best = LOD_CELL_EMPTY;
            for bz in 0..foot {
                for bx in 0..foot {
                    let cell = tops[(cz2 * foot + bz) * 16 + cx2 * foot + bx];
                    if cell.top_y > best.top_y {
                        best = cell;
                    }
                }
            }
            out[cz2 * edge + cx2] = best;
        }
    }
    Some(out)
}

/// The `LOD1` frame a client asking for `(cx, cz)` in `dim` at `resolution`
/// receives, or `None` for an unsupported resolution or unrepresentable
/// coordinate.
fn encode_lod_v1_column(dim: DimId, cx: i32, cz: i32, resolution: u8) -> Option<Vec<u8>> {
    let cells = lod_column_cells(dim, cx, cz, resolution)?;
    encode_lod_v1(cx, cz, resolution, &cells)
}

/// One connection's `REQ1` budget. `last` advances only by whole granted
/// tokens, so a sub-token remainder keeps accruing between requests.
struct LodV1Bucket {
    tokens: u32,
    last: Instant,
}

impl LodV1Bucket {
    fn take(&mut self, now: Instant) -> bool {
        let elapsed_ms = now.saturating_duration_since(self.last).as_millis() as u64;
        let refill = (elapsed_ms * u64::from(LOD_V1_REQ_PER_SEC) / 1000) as u32;
        if refill > 0 {
            self.tokens = (self.tokens + refill).min(LOD_V1_REQ_BURST);
            self.last += Duration::from_millis(
                u64::from(refill) * 1000 / u64::from(LOD_V1_REQ_PER_SEC),
            );
        }
        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }
}

/// The shared `lod_v1` runtime state behind one lock: the encoded-frame cache
/// and every connection's request bucket. Both are leaf-critical sections —
/// nothing else is locked while they are held.
/// `(dim, cx, cz, resolution)` -> `(chunk epoch, inserted, frame)`. A cached
/// hit must match the chunk's live `chunk_version_dim`, so any `set_block*`
/// write in the column retires the frame — the same invalidation rule the
/// chunk packet cache keys on.
type LodV1FrameCache = HashMap<(DimId, i32, i32, u8), (u64, Instant, Vec<u8>)>;

#[derive(Default)]
struct LodV1Shared {
    frames: LodV1FrameCache,
    /// Per-connection `REQ1` buckets keyed by entity id; swept with the rest
    /// of the per-eid session state on disconnect.
    buckets: HashMap<i32, LodV1Bucket>,
}

fn lod_v1_shared() -> &'static Mutex<LodV1Shared> {
    static SHARED: OnceLock<Mutex<LodV1Shared>> = OnceLock::new();
    SHARED.get_or_init(|| Mutex::new(LodV1Shared::default()))
}

/// Drop a disconnected connection's request bucket with the rest of its
/// per-eid state (`forget_per_eid_state` calls this).
fn lod_v1_forget(eid: i32) {
    lod_v1_shared()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .buckets
        .remove(&eid);
}

/// Retire every cached frame for chunk `(cx, cz)` in `dim`; called from
/// `bump_world_epoch_dim` so block writes drop stale columns promptly rather
/// than only on the next epoch-miss.
fn invalidate_lod_v1_chunk(dim: DimId, cx: i32, cz: i32) {
    lod_v1_shared()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .frames
        .retain(|&(d, x, z, _), _| !(d == dim && x == cx && z == cz));
}

/// The cached frame for `key` when it is still at the chunk's live epoch.
fn lod_v1_cached(key: (DimId, i32, i32, u8), epoch: u64) -> Option<Vec<u8>> {
    let shared = lod_v1_shared().lock().unwrap_or_else(|e| e.into_inner());
    match shared.frames.get(&key) {
        Some(&(at_epoch, _, ref frame)) if at_epoch == epoch => Some(frame.clone()),
        _ => None,
    }
}

/// Whether `eid`'s bucket can spend a token now.
fn lod_v1_rate_take(eid: i32, now: Instant) -> bool {
    let mut shared = lod_v1_shared().lock().unwrap_or_else(|e| e.into_inner());
    shared
        .buckets
        .entry(eid)
        .or_insert(LodV1Bucket {
            tokens: LOD_V1_REQ_BURST,
            last: now,
        })
        .take(now)
}

/// Store a freshly encoded frame, evicting the oldest entries at the cap.
/// Stale-epoch entries are harmless dead weight — lookups already re-check the
/// epoch — so plain oldest-first keeps this critical section a leaf (no calls
/// into `chunk_versions`, which `invalidate_lod_v1_chunk` may be holding).
fn lod_v1_cache_insert(key: (DimId, i32, i32, u8), epoch: u64, frame: &[u8], now: Instant) {
    let mut shared = lod_v1_shared().lock().unwrap_or_else(|e| e.into_inner());
    if shared.frames.len() >= LOD_V1_CACHE_CAP && !shared.frames.contains_key(&key) {
        while shared.frames.len() >= LOD_V1_CACHE_CAP {
            let Some(&oldest) = shared
                .frames
                .iter()
                .min_by_key(|(_, (_, at, _))| *at)
                .map(|(k, _)| k)
            else {
                break;
            };
            shared.frames.remove(&oldest);
        }
    }
    shared
        .frames
        .insert(key, (epoch, now, frame.to_vec()));
}

/// `minecraft:register` / `minecraft:unregister` payloads carry the channel
/// list as NUL-separated UTF-8 names; yield each entry as `&str`.
fn registered_channels(payload: &[u8]) -> impl Iterator<Item = &str> {
    payload
        .split(|&b| b == 0)
        .filter(|part| !part.is_empty())
        .filter_map(|part| std::str::from_utf8(part).ok())
}

/// A `minecraft:register` advertising `mc-spine:lod_v1` flips the client's
/// opt-in flag on and is answered with the S2C `0x19` echo naming the channel.
fn lod_v1_register(client: &Client, payload: &[u8]) {
    if !registered_channels(payload).any(|name| name == LOD_V1_CHANNEL) {
        return;
    }
    client.lod_v1_enabled.store(true, Ordering::Relaxed);
    let mut out = Vec::with_capacity(LOD_V1_CHANNEL.len() + 8);
    write_str(&mut out, "minecraft:register");
    out.extend_from_slice(LOD_V1_CHANNEL.as_bytes());
    let _ = send_packet(&mut client.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x19, out);
}

/// A `minecraft:unregister` naming the channel flips the opt-in flag off.
fn lod_v1_unregister(client: &Client, payload: &[u8]) {
    if registered_channels(payload).any(|name| name == LOD_V1_CHANNEL) {
        client.lod_v1_enabled.store(false, Ordering::Relaxed);
    }
}

/// A `REQ1` frame on the channel: 13 bytes
/// (`b"REQ1"`, resolution u8, `cx`/`cz` i32 LE). Anything malformed, out of
/// range, or from a client that never opted in is silently dropped — the
/// channel is opt-in, so dropping is the whole contract.
fn lod_v1_request(client: &Client, payload: &[u8]) {
    if !client.lod_v1_enabled.load(Ordering::Relaxed) {
        return;
    }
    if payload.len() != LOD_V1_REQ_BYTES || !payload.starts_with(LOD_V1_REQ_MAGIC) {
        return;
    }
    let resolution = payload[4];
    if !matches!(resolution, LOD_RES_COARSE | LOD_RES_FINE) {
        return;
    }
    let cx = i32::from_le_bytes(payload[5..9].try_into().expect("request cx"));
    let cz = i32::from_le_bytes(payload[9..13].try_into().expect("request cz"));
    let (px, _, pz, ..) = *client.pos.lock().unwrap_or_else(|e| e.into_inner());
    let near = (i64::from(cx) - i64::from(px.floor() as i32 >> 4))
        .abs()
        .max((i64::from(cz) - i64::from(pz.floor() as i32 >> 4)).abs());
    if near > i64::from(LOD_V1_MAX_RANGE_CHUNKS) {
        return;
    }
    let now = Instant::now();
    // Token bucket per connection: over-budget requests are dropped before any
    // column scan runs, so a REQ1 flood costs nothing but the drop.
    if !lod_v1_rate_take(client.eid, now) {
        return;
    }
    let dim = *client.dim.lock().unwrap_or_else(|e| e.into_inner());
    let key = (dim, cx, cz, resolution);
    let epoch = chunk_version_dim(dim, cx, cz);
    let frame = match lod_v1_cached(key, epoch) {
        Some(frame) => frame,
        None => {
            LOD_V1_COLUMN_SCANS.fetch_add(1, Ordering::Relaxed);
            let Some(frame) = encode_lod_v1_column(dim, cx, cz, resolution) else {
                return;
            };
            // Only cache under the epoch the column still sits at: a mid-scan
            // block write gets correct bytes that are simply not cached — the
            // same rule the chunk packet cache applies.
            if chunk_version_dim(dim, cx, cz) == epoch {
                lod_v1_cache_insert(key, epoch, &frame, now);
            }
            frame
        }
    };
    let mut out = Vec::with_capacity(frame.len() + LOD_V1_CHANNEL.len() + 8);
    write_str(&mut out, LOD_V1_CHANNEL);
    out.extend_from_slice(&frame);
    let _ = send_packet(&mut client.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x19, out);
}

/// C2S `0x12` (Custom Payload) dispatch. `minecraft:register`,
/// `minecraft:unregister` and the `mc-spine:lod_v1` request channel are handled
/// here unconditionally; every other non-`minecraft:` channel keeps flowing to
/// the mod runtime when the `neoforge-events` feature is built.
fn custom_payload_c2s(client: &Client, body: &[u8]) {
    let mut sl = body;
    let Some(len) = read_protocol_i32(&mut sl).and_then(|n| usize::try_from(n).ok()) else {
        return;
    };
    if len > LOD_V1_MAX_CHANNEL || len > sl.len() || sl.len() - len > LOD_V1_MAX_PAYLOAD {
        return;
    }
    let (channel, payload) = sl.split_at(len);
    let Ok(channel) = std::str::from_utf8(channel) else {
        return;
    };
    match channel {
        "minecraft:register" => lod_v1_register(client, payload),
        "minecraft:unregister" => lod_v1_unregister(client, payload),
        LOD_V1_CHANNEL => lod_v1_request(client, payload),
        _ => {
            #[cfg(feature = "neoforge-events")]
            if !channel.starts_with("minecraft:") {
                neoforge_payload(client, body);
            }
        }
    }
}
