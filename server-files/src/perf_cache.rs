// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Zero-side-effect performance caches: each layer is proven bit-for-bit
//! identical to the uncached path it replaces, so enabling them can never
//! change a byte on the wire or a file on disk — only the work spent
//! producing it.
//!
//! Layers:
//!
//! * [`SharedChunkWireCache`] — a keyed cache of fully-encoded, compressed
//!   chunk packets as `Arc<[u8]>`. Fifty players standing in the same base
//!   share one buffer: the encode runs once, every send is an `Arc` clone.
//!   The key carries every rendering/security/protocol discriminator
//!   ([`ChunkWireKey`]), so flipping anti-xray, changing the compression
//!   threshold, or mixing protocol 767/777 clients can never leak a
//!   wrong-mode packet. Explicit [`SharedChunkWireCache::invalidate_chunk`]
//!   plus a hard `max_bytes` LRU bound keep memory capped.
//! * [`DirectionalChunkQueue`] — the view-direction send order for the
//!   `(2d + 1)^2` chunk square around a player: the player's own chunk and
//!   ring 1 first, then increasing squared distance with ties broken toward
//!   the look vector, so elytra flight streams the terrain ahead first. The
//!   emitted set is always the complete Vanilla square — zero drops, zero
//!   duplicates.
//! * [`StartupBinaryCache`] — a hash-verified binary cache for the expensive
//!   datapack/registry parse at startup. A single SHA-256
//!   [`StartupInputFingerprint`] covers the cache schema, data/protocol
//!   versions, the ordered enabled-pack list and every input file's
//!   `(path, len, sha256)`. The `SBC1` envelope is CRC-framed and written
//!   through [`crate::atomic::write_atomic`]; any stale or corrupted file is
//!   a clean [`CacheLookup::Miss`], never stale bytes and never a panic.
//!
//! The module keeps no clock: LRU order is a logical tick, so behavior is
//! deterministic under test.

use crate::atomic::write_atomic;
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::Path;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// SharedChunkWireCache
// ---------------------------------------------------------------------------

/// Cache key for one encoded chunk packet. Every field is a rendering or
/// protocol discriminator: two lookups produce the same packet only when all
/// of them agree, which is exactly when sharing the buffer is correct.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChunkWireKey {
    /// Dimension resource id (`"minecraft:overworld"`, ...).
    pub dimension: String,
    /// Chunk x coordinate.
    pub cx: i32,
    /// Chunk z coordinate.
    pub cz: i32,
    /// Chunk content revision. Any block, light or block-entity edit bumps
    /// it, which changes the key and therefore never returns the stale
    /// packet.
    pub chunk_revision: u64,
    /// Whether anti-xray obfuscation was applied when encoding.
    pub anti_xray_enabled: bool,
    /// Network compression threshold the packet was framed under.
    pub compression_threshold: i32,
    /// Client protocol version (767 for 1.21.1, 777 for 26.3).
    pub protocol_version: i32,
}

/// Snapshot counters for a [`SharedChunkWireCache`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChunkCacheStats {
    /// Lookups served from an existing entry.
    pub hits: u64,
    /// Lookups that had to encode (or could not keep an oversized entry).
    pub misses: u64,
    /// Entries dropped by the `max_bytes` LRU bound. Explicit
    /// `invalidate_chunk` removals are not counted here.
    pub evictions: u64,
    /// Sum of resident packet byte lengths.
    pub resident_bytes: usize,
    /// Number of resident entries.
    pub resident_entries: usize,
}

struct ChunkCacheEntry {
    packet: Arc<[u8]>,
    last_used: u64,
}

/// Bounded LRU cache of encoded chunk packets shared across every recipient.
///
/// `max_bytes` is a hard cap on the sum of resident packet lengths. An entry
/// larger than the cap is returned to the caller but never stored, so a
/// degenerate huge packet cannot evict the whole cache — memory stays
/// strictly bounded either way.
pub struct SharedChunkWireCache {
    entries: HashMap<ChunkWireKey, ChunkCacheEntry>,
    /// `last_used` tick -> key; ticks are unique, so the smallest entry is
    /// always the least recently used one.
    lru: BTreeMap<u64, ChunkWireKey>,
    tick: u64,
    max_bytes: usize,
    resident_bytes: usize,
    hits: u64,
    misses: u64,
    evictions: u64,
}

impl SharedChunkWireCache {
    /// Empty cache keeping at most `max_bytes` of packet payloads.
    pub fn new(max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            lru: BTreeMap::new(),
            tick: 0,
            max_bytes,
            resident_bytes: 0,
            hits: 0,
            misses: 0,
            evictions: 0,
        }
    }

    /// The configured byte cap.
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Resident entry count.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache holds no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Sum of resident packet byte lengths.
    pub fn resident_bytes(&self) -> usize {
        self.resident_bytes
    }

    /// Cumulative counters snapshot.
    pub fn stats(&self) -> ChunkCacheStats {
        ChunkCacheStats {
            hits: self.hits,
            misses: self.misses,
            evictions: self.evictions,
            resident_bytes: self.resident_bytes,
            resident_entries: self.entries.len(),
        }
    }

    /// Probe without encoding: a hit refreshes the entry's LRU position.
    pub fn get(&mut self, key: &ChunkWireKey) -> Option<Arc<[u8]>> {
        if self.entries.contains_key(key) {
            self.hits += 1;
            self.touch(key);
            Some(self.entries[key].packet.clone())
        } else {
            self.misses += 1;
            None
        }
    }

    /// Send path: serve the shared buffer for `key`, running `encode` exactly
    /// once on a miss and storing the result for the next recipient. The
    /// returned `Arc` is byte-identical to what `encode` produced.
    pub fn get_or_insert_with(
        &mut self,
        key: ChunkWireKey,
        encode: impl FnOnce() -> Vec<u8>,
    ) -> Arc<[u8]> {
        if self.entries.contains_key(&key) {
            self.hits += 1;
            self.touch(&key);
            return self.entries[&key].packet.clone();
        }
        self.misses += 1;
        let packet: Arc<[u8]> = encode().into();
        self.insert_arc(key, packet.clone());
        packet
    }

    /// Store a pre-encoded packet under `key`, replacing any existing entry.
    /// Oversized packets (`packet.len() > max_bytes`) are not stored; an
    /// existing entry under the same key is still replaced-removed so a stale
    /// large packet cannot linger.
    pub fn insert(&mut self, key: ChunkWireKey, packet: Arc<[u8]>) {
        self.insert_arc(key, packet);
    }

    fn insert_arc(&mut self, key: ChunkWireKey, packet: Arc<[u8]>) {
        if let Some(old) = self.entries.remove(&key) {
            self.lru.remove(&old.last_used);
            self.resident_bytes -= old.packet.len();
        }
        if packet.len() > self.max_bytes {
            return;
        }
        self.tick = self.tick.wrapping_add(1);
        self.resident_bytes += packet.len();
        self.lru.insert(self.tick, key.clone());
        self.entries.insert(
            key,
            ChunkCacheEntry {
                packet,
                last_used: self.tick,
            },
        );
        self.evict_to_cap();
    }

    /// Drop every cached packet for `(dimension, cx, cz)` regardless of
    /// revision, anti-xray, threshold or protocol discriminator — the
    /// immediate invalidation a block/light/block-entity edit triggers.
    /// Returns the number of entries removed.
    pub fn invalidate_chunk(&mut self, dimension: &str, cx: i32, cz: i32) -> usize {
        let doomed: Vec<ChunkWireKey> = self
            .entries
            .keys()
            .filter(|k| k.dimension == dimension && k.cx == cx && k.cz == cz)
            .cloned()
            .collect();
        let removed = doomed.len();
        for key in doomed {
            if let Some(entry) = self.entries.remove(&key) {
                self.lru.remove(&entry.last_used);
                self.resident_bytes -= entry.packet.len();
            }
        }
        removed
    }

    /// Drop all entries and reset resident accounting; cumulative hit/miss/
    /// eviction counters are kept.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.lru.clear();
        self.resident_bytes = 0;
    }

    fn touch(&mut self, key: &ChunkWireKey) {
        if let Some(entry) = self.entries.get_mut(key) {
            self.lru.remove(&entry.last_used);
            self.tick = self.tick.wrapping_add(1);
            entry.last_used = self.tick;
            self.lru.insert(self.tick, key.clone());
        }
    }

    /// Evict least-recently-used entries until the byte cap holds.
    fn evict_to_cap(&mut self) {
        while self.resident_bytes > self.max_bytes {
            let Some((_, key)) = self.lru.pop_first() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&key) {
                self.resident_bytes -= entry.packet.len();
                self.evictions += 1;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// DirectionalChunkQueue
// ---------------------------------------------------------------------------

/// Horizontal look direction for a player yaw in degrees, using Vanilla's
/// convention (`Entity.getViewVector` at pitch 0): yaw `0` faces `+Z`
/// (south), `90` faces `-X` (west), `180` faces `-Z`, `270` faces `+X`.
/// Non-finite input degenerates to `(0, 0)` — a directionless queue still
/// emits the full square in a deterministic distance order.
pub fn look_dir_from_yaw(yaw_degrees: f64) -> (f64, f64) {
    let rad = yaw_degrees.to_radians();
    (-rad.sin(), rad.cos())
}

/// Send order for the `(2 * view_distance + 1)^2` chunk square around a
/// player: a priority permutation of the exact Vanilla set.
///
/// Ordering contract:
///
/// 1. The player's own chunk and ring 1 (all chunks at Chebyshev distance
///    `<= 1`) come first — guaranteed automatically because every ring-1
///    chunk has squared distance `<= 2` while the nearest ring-2 chunk sits
///    at `4`.
/// 2. Remaining chunks sort by increasing squared distance.
/// 3. Ties break toward the highest forward dot product
///    `dot(dir_to_chunk, look_dir)`, so the chunks a player is flying toward
///    stream before the ones already behind them.
/// 4. Final ties (zero look vector, perpendicular chunks) break on chunk
///    offset for a fully deterministic order.
///
/// The enumeration covers every offset `-d..=d` on both axes exactly once —
/// the emitted set is always the complete Vanilla square with no duplicates
/// and no missing chunks.
pub struct DirectionalChunkQueue {
    order: Vec<(i32, i32)>,
    cursor: usize,
}

impl DirectionalChunkQueue {
    /// Build the order for a player at chunk `(player_cx, player_cz)` looking
    /// along horizontal vector `(look_x, look_z)` (any scale; it is
    /// normalized internally) with Vanilla-style Chebyshev `view_distance`.
    pub fn new(
        player_cx: i32,
        player_cz: i32,
        look_x: f64,
        look_z: f64,
        view_distance: u8,
    ) -> Self {
        let look = normalize_look(look_x, look_z);
        let d = i64::from(view_distance);
        let mut ranked: Vec<(i64, f64, i32, i32)> =
            Vec::with_capacity(((2 * d + 1) * (2 * d + 1)) as usize);
        for dz in -d..=d {
            for dx in -d..=d {
                let dist2 = dx * dx + dz * dz;
                let dot = if dist2 == 0 {
                    0.0
                } else {
                    let inv = 1.0 / (dist2 as f64).sqrt();
                    (dx as f64 * inv) * look.0 + (dz as f64 * inv) * look.1
                };
                ranked.push((dist2, dot, dx as i32, dz as i32));
            }
        }
        ranked.sort_unstable_by(|a, b| {
            a.0.cmp(&b.0)
                // Highest forward dot first: reverse the f64 ordering.
                .then_with(|| b.1.total_cmp(&a.1))
                .then_with(|| a.2.cmp(&b.2))
                .then_with(|| a.3.cmp(&b.3))
        });
        let order = ranked
            .into_iter()
            .map(|(_, _, dx, dz)| (player_cx + dx, player_cz + dz))
            .collect();
        Self { order, cursor: 0 }
    }

    /// Build from a player yaw in degrees (Vanilla convention, see
    /// [`look_dir_from_yaw`]) instead of a raw look vector.
    pub fn from_yaw(player_cx: i32, player_cz: i32, yaw_degrees: f64, view_distance: u8) -> Self {
        let (look_x, look_z) = look_dir_from_yaw(yaw_degrees);
        Self::new(player_cx, player_cz, look_x, look_z, view_distance)
    }

    /// The full ordering, front = send first.
    pub fn ordered(&self) -> &[(i32, i32)] {
        &self.order
    }

    /// Chunks not yet popped.
    pub fn remaining(&self) -> usize {
        self.order.len() - self.cursor
    }

    /// Whether every chunk has been popped.
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// The next chunk to send without popping it.
    pub fn peek(&self) -> Option<(i32, i32)> {
        self.order.get(self.cursor).copied()
    }
}

impl Iterator for DirectionalChunkQueue {
    type Item = (i32, i32);

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.order.get(self.cursor).copied();
        self.cursor += usize::from(item.is_some());
        item
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining(), Some(self.remaining()))
    }
}

impl ExactSizeIterator for DirectionalChunkQueue {}

/// Normalize a horizontal look vector; a zero or non-finite input yields
/// `(0, 0)`, which flattens every dot product to `0` so ordering degenerates
/// deterministically instead of propagating NaN into the comparator.
fn normalize_look(look_x: f64, look_z: f64) -> (f64, f64) {
    let len = (look_x * look_x + look_z * look_z).sqrt();
    if len.is_finite() && len > 0.0 {
        (look_x / len, look_z / len)
    } else {
        (0.0, 0.0)
    }
}

// ---------------------------------------------------------------------------
// Pure-std FIPS 180-4 SHA-256
// ---------------------------------------------------------------------------

const SHA256_H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

fn sha256_compress(h: &mut [u32; 8], block: &[u8]) {
    debug_assert_eq!(block.len(), 64);
    let mut w = [0u32; 64];
    for i in 0..16 {
        w[i] = u32::from_be_bytes([
            block[4 * i],
            block[4 * i + 1],
            block[4 * i + 2],
            block[4 * i + 3],
        ]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
        (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let t1 = hh
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(SHA256_K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    h[0] = h[0].wrapping_add(a);
    h[1] = h[1].wrapping_add(b);
    h[2] = h[2].wrapping_add(c);
    h[3] = h[3].wrapping_add(d);
    h[4] = h[4].wrapping_add(e);
    h[5] = h[5].wrapping_add(f);
    h[6] = h[6].wrapping_add(g);
    h[7] = h[7].wrapping_add(hh);
}

/// Streaming pure-`std` FIPS 180-4 SHA-256 hasher — zero crates.io
/// dependencies. Feed bytes with [`Sha256::update`] in any chunking and take
/// the digest with [`Sha256::finalize`].
struct Sha256 {
    h: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total_len: u64,
}

impl Sha256 {
    fn new() -> Self {
        Self {
            h: SHA256_H0,
            buf: [0u8; 64],
            buf_len: 0,
            total_len: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                sha256_compress(&mut self.h, &block);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            sha256_compress(&mut self.h, &data[..64]);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    fn finalize(mut self) -> [u8; 32] {
        let bit_len = self.total_len.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buf_len != 56 {
            self.update(&[0]);
        }
        self.buf[56..64].copy_from_slice(&bit_len.to_be_bytes());
        let block = self.buf;
        sha256_compress(&mut self.h, &block);
        let mut out = [0u8; 32];
        for (i, word) in self.h.iter().enumerate() {
            out[4 * i..4 * i + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize()
}

// ---------------------------------------------------------------------------
// IEEE CRC32
// ---------------------------------------------------------------------------

/// IEEE 802.3 reflected CRC32 (the Ethernet/`zlib` polynomial), built at
/// compile time — pure `std`, no table crate.
const fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC32_TABLE: [u32; 256] = crc32_table();

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc = CRC32_TABLE[((crc ^ u32::from(b)) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

// ---------------------------------------------------------------------------
// StartupBinaryCache
// ---------------------------------------------------------------------------

/// On-disk cache file magic.
pub const STARTUP_CACHE_MAGIC: [u8; 4] = *b"SBC1";

/// Bumped whenever the fingerprint preimage or the payload interpretation
/// changes, so a cache written by another build is never trusted.
pub const CACHE_SCHEMA_VERSION: u32 = 1;

const ENVELOPE_HEADER_LEN: usize = 4 + 4 + 32 + 4;
const ENVELOPE_FOOTER_LEN: usize = 4;

/// One hashed input to the startup fingerprint: an already-read file reduced
/// to `(relative_path, byte_len, sha256)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartupInputFile {
    /// Path relative to the server root, `/`-separated (e.g.
    /// `world/datapacks/base/data/x/recipes/y.json`).
    pub relative_path: String,
    /// File length in bytes.
    pub byte_len: u64,
    /// FIPS 180-4 SHA-256 of the file's exact bytes.
    pub sha256: [u8; 32],
}

impl StartupInputFile {
    /// Hash `contents` into a fingerprint input — a convenience for callers
    /// that already hold the file bytes.
    pub fn from_contents(relative_path: impl Into<String>, contents: &[u8]) -> Self {
        Self {
            relative_path: relative_path.into(),
            byte_len: contents.len() as u64,
            sha256: sha256(contents),
        }
    }
}

/// The 32-byte digest covering every input a cached startup payload was
/// derived from: cache schema, data version, protocol version, the ordered
/// enabled-pack list, and every input file's `(path, len, sha256)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StartupInputFingerprint(pub [u8; 32]);

impl StartupInputFingerprint {
    /// Raw digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Why a [`StartupBinaryCache::load_if_fresh`] declined the stored file.
/// Every variant means "recompute and overwrite" — no variant hands out
/// bytes the caller did not ask for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheMissReason {
    /// The file could not be read (absent, permission, ...). Carries the
    /// `io::ErrorKind` (`NotFound` for a first-boot miss).
    Io(io::ErrorKind),
    /// Bytes on disk failed framing: truncated, wrong magic, bad length
    /// field, or IEEE CRC32 mismatch — the fail-closed answer for any
    /// flipped bit.
    Corrupt,
    /// The envelope is intact but was written under a different
    /// [`CACHE_SCHEMA_VERSION`].
    SchemaMismatch {
        /// The schema stored in the file.
        found: u32,
    },
    /// The envelope is intact but the stored fingerprint differs from the
    /// expected one: an input file changed, pack order changed, or the
    /// data/protocol version differs.
    FingerprintMismatch,
}

/// Result of [`StartupBinaryCache::load_if_fresh`].
#[derive(Debug)]
pub enum CacheLookup {
    /// Fresh payload, byte-identical to what was saved.
    Hit(Vec<u8>),
    /// Not usable; recompute and overwrite. Carries the reason for
    /// observability.
    Miss(CacheMissReason),
}

impl CacheLookup {
    /// `Some(payload)` on a hit, `None` on any miss.
    pub fn into_payload(self) -> Option<Vec<u8>> {
        match self {
            CacheLookup::Hit(bytes) => Some(bytes),
            CacheLookup::Miss(_) => None,
        }
    }
}

/// Hash-verified binary cache for the startup datapack/registry parse.
///
/// Envelope `SBC1`:
///
/// ```text
/// [4B magic "SBC1"][u32 BE schema][32B input fingerprint]
/// [u32 BE payload_len][payload][u32 BE ieee_crc32 over all prior bytes]
/// ```
///
/// The CRC covers the header too, so a flipped bit anywhere — magic, schema,
/// fingerprint, length or payload — is detected before any field is read.
/// Writes go through [`write_atomic`], so a crash leaves the old cache or
/// the new one, never a torn envelope.
pub struct StartupBinaryCache;

impl StartupBinaryCache {
    /// Deterministic fingerprint of the startup inputs.
    ///
    /// `enabled_packs` is order-sensitive on purpose: `/datapack enable
    /// before|after` changes override semantics, so reordering must
    /// invalidate. `files` is sorted internally by `(relative_path,
    /// byte_len, sha256)`, so caller enumeration order does not matter —
    /// only the content set does.
    pub fn compute_fingerprint(
        data_version: i32,
        protocol_version: i32,
        enabled_packs: &[String],
        files: &[StartupInputFile],
    ) -> StartupInputFingerprint {
        let mut sorted: Vec<&StartupInputFile> = files.iter().collect();
        sorted.sort_by(|a, b| {
            a.relative_path
                .cmp(&b.relative_path)
                .then_with(|| a.byte_len.cmp(&b.byte_len))
                .then_with(|| a.sha256.cmp(&b.sha256))
        });

        let mut h = Sha256::new();
        h.update(&CACHE_SCHEMA_VERSION.to_be_bytes());
        h.update(&data_version.to_be_bytes());
        h.update(&protocol_version.to_be_bytes());
        h.update(&(enabled_packs.len() as u32).to_be_bytes());
        for name in enabled_packs {
            h.update(&(name.len() as u32).to_be_bytes());
            h.update(name.as_bytes());
        }
        h.update(&(sorted.len() as u32).to_be_bytes());
        for file in sorted {
            h.update(&(file.relative_path.len() as u32).to_be_bytes());
            h.update(file.relative_path.as_bytes());
            h.update(&file.byte_len.to_be_bytes());
            h.update(&file.sha256);
        }
        StartupInputFingerprint(h.finalize())
    }

    /// Serialize the `SBC1` envelope for `payload` under an arbitrary schema
    /// number — exposed so migration tooling and tests can construct
    /// envelopes this build would write, or ones it must reject. Payloads
    /// above `u32::MAX` bytes cannot be framed; [`Self::save`] refuses them.
    pub fn encode_envelope(
        schema_version: u32,
        fingerprint: &StartupInputFingerprint,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::with_capacity(ENVELOPE_HEADER_LEN + payload.len() + ENVELOPE_FOOTER_LEN);
        out.extend_from_slice(&STARTUP_CACHE_MAGIC);
        out.extend_from_slice(&schema_version.to_be_bytes());
        out.extend_from_slice(fingerprint.as_bytes());
        let len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(payload);
        let crc = crc32(&out);
        out.extend_from_slice(&crc.to_be_bytes());
        out
    }

    /// Atomically write `payload` to `path` framed as `SBC1` under
    /// [`CACHE_SCHEMA_VERSION`] and `fingerprint`.
    pub fn save(
        path: &Path,
        fingerprint: &StartupInputFingerprint,
        payload: &[u8],
    ) -> io::Result<()> {
        if u32::try_from(payload.len()).is_err() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SBC1 payload exceeds u32 length field",
            ));
        }
        let envelope = Self::encode_envelope(CACHE_SCHEMA_VERSION, fingerprint, payload);
        write_atomic(path, &envelope)
    }

    /// Read `path` and return its payload only when the envelope is intact
    /// and the stored fingerprint equals `expected`.
    ///
    /// Fail-closed in both directions: a corrupted file is a miss even if
    /// its surviving fields look plausible, and a wrong fingerprint is a
    /// miss even if the payload is otherwise well-formed. Never panics —
    /// every bounds check returns [`CacheLookup::Miss`].
    pub fn load_if_fresh(path: &Path, expected: &StartupInputFingerprint) -> CacheLookup {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) => return CacheLookup::Miss(CacheMissReason::Io(e.kind())),
        };
        if bytes.len() < ENVELOPE_HEADER_LEN + ENVELOPE_FOOTER_LEN {
            return CacheLookup::Miss(CacheMissReason::Corrupt);
        }
        let (body, crc_field) = bytes.split_at(bytes.len() - ENVELOPE_FOOTER_LEN);
        let stored_crc =
            u32::from_be_bytes([crc_field[0], crc_field[1], crc_field[2], crc_field[3]]);
        if crc32(body) != stored_crc {
            return CacheLookup::Miss(CacheMissReason::Corrupt);
        }
        if body[..4] != STARTUP_CACHE_MAGIC {
            return CacheLookup::Miss(CacheMissReason::Corrupt);
        }
        let schema = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
        if schema != CACHE_SCHEMA_VERSION {
            return CacheLookup::Miss(CacheMissReason::SchemaMismatch { found: schema });
        }
        if body[8..40] != expected.0[..] {
            return CacheLookup::Miss(CacheMissReason::FingerprintMismatch);
        }
        let payload_len = u32::from_be_bytes([body[40], body[41], body[42], body[43]]) as usize;
        if body.len() != ENVELOPE_HEADER_LEN + payload_len {
            return CacheLookup::Miss(CacheMissReason::Corrupt);
        }
        CacheLookup::Hit(body[ENVELOPE_HEADER_LEN..].to_vec())
    }
}
