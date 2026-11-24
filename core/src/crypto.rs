// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Canonical, zero-dependency cryptographic digests the whole workspace
//! shares: FIPS 180-4 [`Sha256`] and [`Sha1`], and RFC 1321 [`Md5`].
//!
//! Each algorithm exposes the same three shapes:
//!
//! * a streaming hasher ([`Sha256::new`] / [`Sha256::update`] /
//!   [`Sha256::finalize`]) so multi-megabyte payloads hash in bounded memory
//!   and split feeds produce the identical digest,
//! * a one-shot helper ([`sha256`], [`sha1`], [`md5`]) returning the raw
//!   digest bytes,
//! * hex renderings ([`sha256_hex`], [`sha1_hex`], [`md5_hex`]) built on the
//!   shared [`hex_encode`].
//!
//! Minecraft-specific derivations live here too so every lane stops
//! re-implementing them: [`offline_player_uuid`] is vanilla's
//! `UUID.nameUUIDFromBytes("OfflinePlayer:" + name)` offline-mode identity,
//! [`minecraft_sha1_hex`] and [`sha1_signed_hex`] render digests the way
//! Java's `BigInteger.toString(16)` does for the `hasJoined` server hash, and
//! [`sha256_file`] streams a file's digest for installer/integrity checks.
//!
//! SHA-1 and MD5 are kept only for protocol parity (resource-pack pins,
//! Mojang's server hash, offline UUIDs); they are not collision-resistant
//! and must never guard new trust decisions — use [`Sha256`] there.

use std::io::{self, Read};
use std::path::Path;

/// Lowercase hex of `bytes` — the rendering every digest helper shares.
pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

// ============================ SHA-256 (FIPS 180-4) ============================

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

/// Streaming FIPS 180-4 SHA-256 hasher.
///
/// `update` may be called with any chunk sizes — partial blocks are buffered
/// and the digest depends only on the concatenated byte stream. `finalize`
/// consumes the hasher.
#[derive(Clone)]
pub struct Sha256 {
    state: [u32; 8],
    block: [u8; 64],
    block_len: usize,
    total: u64,
}

impl Sha256 {
    /// A fresh hasher at the FIPS 180-4 initial state.
    pub fn new() -> Self {
        Self {
            state: SHA256_H0,
            block: [0u8; 64],
            block_len: 0,
            total: 0,
        }
    }

    fn compress(state: &mut [u32; 8], block: &[u8]) {
        debug_assert_eq!(block.len(), 64);
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
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
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
        for i in 0..64 {
            let bs1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(bs1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let bs0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = bs0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (acc, v) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *acc = acc.wrapping_add(v);
        }
    }

    /// Feed more bytes. Chunk boundaries are irrelevant to the result.
    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        if self.block_len > 0 {
            let take = (64 - self.block_len).min(data.len());
            self.block[self.block_len..self.block_len + take].copy_from_slice(&data[..take]);
            self.block_len += take;
            data = &data[take..];
            if self.block_len == 64 {
                let block = self.block;
                Self::compress(&mut self.state, &block);
                self.block_len = 0;
            }
        }
        while data.len() >= 64 {
            Self::compress(&mut self.state, &data[..64]);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.block[..data.len()].copy_from_slice(data);
            self.block_len = data.len();
        }
    }

    /// Pad, finalize, and return the 32-byte digest.
    pub fn finalize(mut self) -> [u8; 32] {
        let bit_len = self.total.wrapping_mul(8);
        self.update(&[0x80]);
        while self.block_len != 56 {
            self.update(&[0]);
        }
        self.update(&bit_len.to_be_bytes());
        let mut out = [0u8; 32];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    /// [`finalize`](Self::finalize) rendered as 64 lowercase hex chars.
    pub fn finalize_hex(self) -> String {
        hex_encode(&self.finalize())
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

/// SHA-256 of `data` as raw bytes.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize()
}

/// SHA-256 of `data` as lowercase hex.
pub fn sha256_hex(data: &[u8]) -> String {
    hex_encode(&sha256(data))
}

/// SHA-256 of a file's contents as lowercase hex, streamed in bounded chunks.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut chunk)?;
        if read == 0 {
            return Ok(hasher.finalize_hex());
        }
        hasher.update(&chunk[..read]);
    }
}

// ============================= SHA-1 (FIPS 180-4) =============================

const SHA1_H0: [u32; 5] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0];

/// Streaming FIPS 180-4 SHA-1 hasher.
///
/// Retained for protocol parity (resource-pack SHA-1 pins, Mojang's signed
/// `hasJoined` hash); do not use it where collision resistance matters.
#[derive(Clone)]
pub struct Sha1 {
    state: [u32; 5],
    block: [u8; 64],
    block_len: usize,
    total: u64,
}

impl Sha1 {
    /// A fresh hasher at the FIPS 180-4 initial state.
    pub fn new() -> Self {
        Self {
            state: SHA1_H0,
            block: [0u8; 64],
            block_len: 0,
            total: 0,
        }
    }

    fn compress(state: &mut [u32; 5], block: &[u8]) {
        debug_assert_eq!(block.len(), 64);
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) =
            (state[0], state[1], state[2], state[3], state[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5a827999u32),
                20..=39 => (b ^ c ^ d, 0x6ed9eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
                _ => (b ^ c ^ d, 0xca62c1d6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
    }

    /// Feed more bytes. Chunk boundaries are irrelevant to the result.
    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        if self.block_len > 0 {
            let take = (64 - self.block_len).min(data.len());
            self.block[self.block_len..self.block_len + take].copy_from_slice(&data[..take]);
            self.block_len += take;
            data = &data[take..];
            if self.block_len == 64 {
                let block = self.block;
                Self::compress(&mut self.state, &block);
                self.block_len = 0;
            }
        }
        while data.len() >= 64 {
            Self::compress(&mut self.state, &data[..64]);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.block[..data.len()].copy_from_slice(data);
            self.block_len = data.len();
        }
    }

    /// Pad, finalize, and return the 20-byte digest.
    pub fn finalize(mut self) -> [u8; 20] {
        let bit_len = self.total.wrapping_mul(8);
        self.update(&[0x80]);
        while self.block_len != 56 {
            self.update(&[0]);
        }
        self.update(&bit_len.to_be_bytes());
        let mut out = [0u8; 20];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    /// [`finalize`](Self::finalize) rendered as 40 lowercase hex chars.
    pub fn finalize_hex(self) -> String {
        hex_encode(&self.finalize())
    }
}

impl Default for Sha1 {
    fn default() -> Self {
        Self::new()
    }
}

/// SHA-1 of `data` as raw bytes.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(data);
    hasher.finalize()
}

/// SHA-1 of `data` as lowercase hex.
pub fn sha1_hex(data: &[u8]) -> String {
    hex_encode(&sha1(data))
}

/// A SHA-1 digest rendered the way Java's `new BigInteger(digest).toString(16)`
/// prints it: big-endian two's complement, a leading `-` when the top bit is
/// set, magnitude with leading zeros stripped. This is the format Mojang's
/// `hasJoined` endpoint expects the server hash in.
pub fn sha1_signed_hex(digest: &[u8; 20]) -> String {
    let negative = digest[0] & 0x80 != 0;
    let mut bytes = *digest;
    if negative {
        let mut carry = 1u16;
        for b in bytes.iter_mut().rev() {
            let v = u16::from(!*b) + carry;
            *b = v as u8;
            carry = v >> 8;
        }
    }
    let digits = hex_encode(&bytes);
    let trimmed = digits.trim_start_matches('0');
    let magnitude = if trimmed.is_empty() { "0" } else { trimmed };
    if negative {
        format!("-{magnitude}")
    } else {
        magnitude.to_string()
    }
}

/// SHA-1 of `data` rendered as Java's signed `BigInteger.toString(16)` — the
/// server-id hash form Mojang's `hasJoined` lookup and the vanilla client's
/// session hash both use.
pub fn minecraft_sha1_hex(data: &[u8]) -> String {
    sha1_signed_hex(&sha1(data))
}

// =============================== MD5 (RFC 1321) ===============================

const MD5_S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

const MD5_K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

const MD5_H0: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];

/// Streaming RFC 1321 MD5 hasher.
///
/// Retained for protocol parity (offline-mode UUIDs, `fromHashOf` seed
/// mixing); MD5 is broken and must never gate trust decisions.
#[derive(Clone)]
pub struct Md5 {
    state: [u32; 4],
    block: [u8; 64],
    block_len: usize,
    total: u64,
}

impl Md5 {
    /// A fresh hasher at the RFC 1321 initial state.
    pub fn new() -> Self {
        Self {
            state: MD5_H0,
            block: [0u8; 64],
            block_len: 0,
            total: 0,
        }
    }

    fn compress(state: &mut [u32; 4], block: &[u8]) {
        debug_assert_eq!(block.len(), 64);
        let mut m = [0u32; 16];
        for (i, word) in m.iter_mut().enumerate() {
            *word = u32::from_le_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        let (mut a, mut b, mut c, mut d) = (state[0], state[1], state[2], state[3]);
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | (!b & d), i),
                16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let tmp = d;
            d = c;
            c = b;
            b = b.wrapping_add(
                a.wrapping_add(f)
                    .wrapping_add(MD5_K[i])
                    .wrapping_add(m[g])
                    .rotate_left(MD5_S[i]),
            );
            a = tmp;
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
    }

    /// Feed more bytes. Chunk boundaries are irrelevant to the result.
    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        if self.block_len > 0 {
            let take = (64 - self.block_len).min(data.len());
            self.block[self.block_len..self.block_len + take].copy_from_slice(&data[..take]);
            self.block_len += take;
            data = &data[take..];
            if self.block_len == 64 {
                let block = self.block;
                Self::compress(&mut self.state, &block);
                self.block_len = 0;
            }
        }
        while data.len() >= 64 {
            Self::compress(&mut self.state, &data[..64]);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.block[..data.len()].copy_from_slice(data);
            self.block_len = data.len();
        }
    }

    /// Pad (little-endian bit length, per RFC 1321), finalize, and return the
    /// 16-byte digest.
    pub fn finalize(mut self) -> [u8; 16] {
        let bit_len = self.total.wrapping_mul(8);
        self.update(&[0x80]);
        while self.block_len != 56 {
            self.update(&[0]);
        }
        self.update(&bit_len.to_le_bytes());
        let mut out = [0u8; 16];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        out
    }

    /// [`finalize`](Self::finalize) rendered as 32 lowercase hex chars.
    pub fn finalize_hex(self) -> String {
        hex_encode(&self.finalize())
    }
}

impl Default for Md5 {
    fn default() -> Self {
        Self::new()
    }
}

/// MD5 of `data` as raw bytes.
pub fn md5(data: &[u8]) -> [u8; 16] {
    let mut hasher = Md5::new();
    hasher.update(data);
    hasher.finalize()
}

/// MD5 of `data` as lowercase hex.
pub fn md5_hex(data: &[u8]) -> String {
    hex_encode(&md5(data))
}

/// Vanilla's offline-mode identity: `UUID.nameUUIDFromBytes` of
/// `"OfflinePlayer:" + name` — an MD5 whose bytes then get the UUID v3
/// version nibble and RFC 4122 variant bits stamped in.
pub fn offline_player_uuid(name: &str) -> [u8; 16] {
    let mut input = Vec::with_capacity(14 + name.len());
    input.extend_from_slice(b"OfflinePlayer:");
    input.extend_from_slice(name.as_bytes());
    let mut digest = md5(&input);
    digest[6] = (digest[6] & 0x0f) | 0x30;
    digest[8] = (digest[8] & 0x3f) | 0x80;
    digest
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `data` through a streaming hasher in `chunk`-byte pieces.
    fn chunked_sha256(data: &[u8], chunk: usize) -> [u8; 32] {
        let mut h = Sha256::new();
        for piece in data.chunks(chunk.max(1)) {
            h.update(piece);
        }
        h.finalize()
    }

    fn chunked_sha1(data: &[u8], chunk: usize) -> [u8; 20] {
        let mut h = Sha1::new();
        for piece in data.chunks(chunk.max(1)) {
            h.update(piece);
        }
        h.finalize()
    }

    fn chunked_md5(data: &[u8], chunk: usize) -> [u8; 16] {
        let mut h = Md5::new();
        for piece in data.chunks(chunk.max(1)) {
            h.update(piece);
        }
        h.finalize()
    }

    #[test]
    fn hex_encode_is_lowercase_and_complete() {
        assert_eq!(hex_encode(&[]), "");
        assert_eq!(hex_encode(&[0x00, 0xab, 0x10, 0xff]), "00ab10ff");
    }

    #[test]
    fn sha256_matches_fips_180_4_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // The 448-bit (two-block) FIPS vector.
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // The one-million-'a' FIPS vector.
        assert_eq!(
            sha256_hex(&vec![b'a'; 1_000_000]),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn sha256_streaming_matches_one_shot_at_block_boundaries() {
        // Sizes straddling the 64-byte block and the 56-byte padding seams.
        for len in [0usize, 1, 55, 56, 57, 63, 64, 65, 119, 120, 127, 128, 129, 1024] {
            let data: Vec<u8> = (0..len).map(|i| (i * 31 + 7) as u8).collect();
            let want = sha256(&data);
            for chunk in [1usize, 3, 7, 55, 56, 64, 65, 200] {
                assert_eq!(chunked_sha256(&data, chunk), want, "len {len} chunk {chunk}");
            }
        }
    }

    #[test]
    fn sha1_matches_fips_180_4_vectors() {
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha1_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        assert_eq!(
            sha1_hex(&vec![b'a'; 1_000_000]),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
    }

    #[test]
    fn sha1_streaming_matches_one_shot_at_block_boundaries() {
        for len in [0usize, 1, 55, 56, 63, 64, 65, 128, 1000] {
            let data: Vec<u8> = (0..len).map(|i| (i * 17 + 3) as u8).collect();
            let want = sha1(&data);
            for chunk in [1usize, 5, 56, 64, 100] {
                assert_eq!(chunked_sha1(&data, chunk), want, "len {len} chunk {chunk}");
            }
        }
    }

    #[test]
    fn md5_matches_the_rfc_1321_suite() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"a"), "0cc175b9c0f1b6a831c399e269772661");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(md5_hex(b"message digest"), "f96b697d7cb7938d525a2f31aaf161d0");
        assert_eq!(
            md5_hex(b"abcdefghijklmnopqrstuvwxyz"),
            "c3fcd3d76192e4007dfb496cca67e13b"
        );
        assert_eq!(
            md5_hex(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"),
            "d174ab98d277d9f5a5611c2c9f419d9f"
        );
        assert_eq!(
            md5_hex(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            ),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
        assert_eq!(md5_hex(&vec![b'a'; 1_000_000]), "7707d6ae4e027c70eea2a935c2296f21");
    }

    #[test]
    fn md5_streaming_matches_one_shot_at_block_boundaries() {
        for len in [0usize, 1, 55, 56, 63, 64, 65, 128, 1000] {
            let data: Vec<u8> = (0..len).map(|i| (i * 13 + 11) as u8).collect();
            let want = md5(&data);
            for chunk in [1usize, 9, 56, 64, 100] {
                assert_eq!(chunked_md5(&data, chunk), want, "len {len} chunk {chunk}");
            }
        }
    }

    /// wiki.vg / vanilla `BigInteger(digest).toString(16)` vectors, including
    /// the negative two's-complement case the plain hex path cannot express.
    #[test]
    fn minecraft_sha1_hex_matches_official_vectors() {
        assert_eq!(
            minecraft_sha1_hex(b"Notch"),
            "4ed1f46bbe04bc756bcb17c0c7ce3e4632f06a48"
        );
        assert_eq!(
            minecraft_sha1_hex(b"jeb_"),
            "-7c9d5b0044c130109a5d7b5fb5c317c02b4e28c1"
        );
        assert_eq!(
            minecraft_sha1_hex(b"simon"),
            "88e16a1019277b15d58faf0541e11910eb756f6"
        );
        // Zero digest renders "0", not an empty string.
        assert_eq!(sha1_signed_hex(&[0; 20]), "0");
    }

    #[test]
    fn offline_player_uuid_matches_java_name_uuid_from_bytes() {
        // `UUID.nameUUIDFromBytes("OfflinePlayer:<name>")`, computed
        // independently with a reference MD5.
        assert_eq!(
            hex_encode(&offline_player_uuid("Notch")),
            "b50ad385829d3141a2167e7d7539ba7f"
        );
        assert_eq!(
            hex_encode(&offline_player_uuid("Steve")),
            "5627dd98e6be3c21b8a8e92344183641"
        );
        assert_eq!(
            hex_encode(&offline_player_uuid("jeb_")),
            "a762f5604fce3236812ab80efff0b62b"
        );
        // Version 3 (name-based MD5) and the RFC 4122 variant bits are set.
        let uuid = offline_player_uuid("Alex");
        assert_eq!(uuid[6] >> 4, 3);
        assert_eq!(uuid[8] >> 6, 0b10);
        assert_eq!(hex_encode(&uuid), "36532b5ec4423dbba24cc7e55d0f979a");
    }

    #[test]
    fn sha256_file_streams_the_digest() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("megu_crypto_test_{:?}", std::process::id()));
        let payload: Vec<u8> = (0..200_000).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &payload).unwrap();
        let hex = sha256_file(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(hex, sha256_hex(&payload));
        assert!(sha256_file(&path).is_err());
    }
}
