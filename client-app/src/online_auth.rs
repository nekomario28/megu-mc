// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Clean-room client-side online-mode login cryptography for Minecraft 1.21.1
//! (Protocol 767) — pure `std`, zero third-party crates, `#![deny(unsafe_code)]`.
//!
//! When the server answers `login_start` with a `0x01 Encryption Request`, the
//! client must, in order:
//!
//! 1. draw a fresh 16-byte shared secret from the OS CSPRNG;
//! 2. compute the signed SHA-1 `serverId` hash Mojang's session server expects
//!    (`server_id || shared_secret || public_key_der`, printed exactly like
//!    Java's `BigInteger(digest).toString(16)` — a leading `-` when the top bit
//!    is set and the magnitude taken as two's complement);
//! 3. when `should_authenticate` is set, POST `{accessToken, selectedProfile,
//!    serverId}` to `https://sessionserver.mojang.com/session/minecraft/join`
//!    and fail closed unless it answers HTTP 204;
//! 4. RSA-PKCS#1-v1.5 encrypt the shared secret and the echoed `verify_token`
//!    under the server's DER `SubjectPublicKeyInfo` key and reply with
//!    `0x01 Encryption Response`;
//! 5. run every later byte through AES-128 in the CFB8 stream mode Java uses
//!    (`AES/CFB8/NoPadding`, key == IV == shared secret).
//!
//! Secret hygiene: `accessToken` and the shared secret are never printed or
//! formatted into errors — [`SessionJoinRequest`]'s `Debug` impl redacts, and
//! the only error strings this module produces carry no secret bytes.

#![deny(unsafe_code)]

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::crash_reporter::{ReportTransport, StdHttpReportTransport};

// ============================ OS CSPRNG =====================================

/// Fill `buf` from the operating-system CSPRNG. On unix targets this reads
/// `/dev/urandom`; there is no pure-`std` CSPRNG elsewhere (Windows'
/// `BCryptGenRandom` requires `unsafe` FFI, which this crate forbids), so
/// non-unix builds fail closed. Tests inject a deterministic source through
/// [`OnlineAuthOptions::rng`] instead of touching this path.
#[cfg(unix)]
pub fn fill_os_random(buf: &mut [u8]) -> Result<(), String> {
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, buf))
        .map_err(|e| format!("OS CSPRNG (/dev/urandom) failed: {e}"))
}

/// Non-unix fallback — always fails closed; see the unix implementation.
#[cfg(not(unix))]
pub fn fill_os_random(_buf: &mut [u8]) -> Result<(), String> {
    Err("OS CSPRNG is only implemented for unix targets (/dev/urandom)".to_string())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The `serverId` hash Mojang's `join`/`hasJoined` endpoints bind to this
/// login: `SHA-1(server_id || shared_secret || public_key_der)` printed in
/// signed Java form. Vanilla always passes an empty `server_id` in 1.21.x.
pub fn mc_server_hash(server_id: &str, shared_secret: &[u8], public_key_der: &[u8]) -> String {
    let mut data = Vec::with_capacity(server_id.len() + shared_secret.len() + public_key_der.len());
    data.extend_from_slice(server_id.as_bytes());
    data.extend_from_slice(shared_secret);
    data.extend_from_slice(public_key_der);
    megu_core::crypto::minecraft_sha1_hex(&data)
}

// ================= AES-128 (FIPS-197) in CFB8 mode ===========================
//
// Minecraft encrypts login onward with AES/CFB8/NoPadding: a 128-bit block
// cipher whose keystream byte 0 is XORed into one data byte at a time, with
// the produced ciphertext byte shifted into the 16-byte feedback register.
// Only the forward cipher is ever needed — decryption reuses it.

const AES_SBOX: [u8; 256] = [
    0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab, 0x76,
    0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4, 0x72, 0xc0,
    0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71, 0xd8, 0x31, 0x15,
    0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2, 0xeb, 0x27, 0xb2, 0x75,
    0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6, 0xb3, 0x29, 0xe3, 0x2f, 0x84,
    0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb, 0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf,
    0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45, 0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8,
    0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5, 0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2,
    0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44, 0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73,
    0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a, 0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb,
    0xe0, 0x32, 0x3a, 0x0a, 0x49, 0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79,
    0xe7, 0xc8, 0x37, 0x6d, 0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08,
    0xba, 0x78, 0x25, 0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a,
    0x70, 0x3e, 0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e,
    0xe1, 0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
    0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb, 0x16,
];
const AES_RCON: [u8; 10] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36];

/// GF(2^8) doubling modulo the AES polynomial.
fn aes_xtime(x: u8) -> u8 {
    if x & 0x80 != 0 {
        (x << 1) ^ 0x1b
    } else {
        x << 1
    }
}

/// FIPS-197 §5.2 key expansion: 11 round keys of 16 bytes for AES-128.
fn aes128_expand_key(key: &[u8; 16]) -> [u8; 176] {
    let mut w = [0u8; 176];
    w[..16].copy_from_slice(key);
    let mut i = 16;
    let mut round = 0usize;
    while i < 176 {
        let mut t = [w[i - 4], w[i - 3], w[i - 2], w[i - 1]];
        if i % 16 == 0 {
            t = [
                AES_SBOX[t[1] as usize] ^ AES_RCON[round],
                AES_SBOX[t[2] as usize],
                AES_SBOX[t[3] as usize],
                AES_SBOX[t[0] as usize],
            ];
            round += 1;
        }
        for j in 0..4 {
            w[i + j] = w[i - 16 + j] ^ t[j];
        }
        i += 4;
    }
    w
}

/// One FIPS-197 forward cipher block under a pre-expanded round key. State is
/// column-major: index `4 * col + row`.
fn aes128_encrypt_block_rk(round_keys: &[u8; 176], input: &[u8; 16]) -> [u8; 16] {
    let mut s = [0u8; 16];
    for i in 0..16 {
        s[i] = input[i] ^ round_keys[i];
    }
    for round in 1..10 {
        // SubBytes fused with ShiftRows: row r at output column c reads input
        // column (c + r) mod 4.
        let mut t = [0u8; 16];
        for c in 0..4 {
            for r in 0..4 {
                t[4 * c + r] = AES_SBOX[s[4 * ((c + r) % 4) + r] as usize];
            }
        }
        // MixColumns: [2 3 1 1; 1 2 3 1; 1 1 2 3; 3 1 1 2] over GF(2^8).
        for c in 0..4 {
            let (a0, a1, a2, a3) = (t[4 * c], t[4 * c + 1], t[4 * c + 2], t[4 * c + 3]);
            s[4 * c] = aes_xtime(a0) ^ (aes_xtime(a1) ^ a1) ^ a2 ^ a3;
            s[4 * c + 1] = a0 ^ aes_xtime(a1) ^ (aes_xtime(a2) ^ a2) ^ a3;
            s[4 * c + 2] = a0 ^ a1 ^ aes_xtime(a2) ^ (aes_xtime(a3) ^ a3);
            s[4 * c + 3] = (aes_xtime(a0) ^ a0) ^ a1 ^ a2 ^ aes_xtime(a3);
        }
        for i in 0..16 {
            s[i] ^= round_keys[16 * round + i];
        }
    }
    let mut out = [0u8; 16];
    for c in 0..4 {
        for r in 0..4 {
            out[4 * c + r] = AES_SBOX[s[4 * ((c + r) % 4) + r] as usize];
        }
    }
    for i in 0..16 {
        out[i] ^= round_keys[160 + i];
    }
    out
}

/// One FIPS-197 forward cipher block from a raw 16-byte key. Exposed for
/// known-answer tests; the transport stream uses [`Aes128Cfb8`].
pub fn aes128_encrypt_block(key: &[u8; 16], input: &[u8; 16]) -> [u8; 16] {
    aes128_encrypt_block_rk(&aes128_expand_key(key), input)
}

/// AES-128/CFB8 stream state — Minecraft's exact `AES/CFB8/NoPadding` with the
/// shared secret as both key and IV. One direction per instance: a connection
/// needs one encryptor for outbound bytes and one decryptor for inbound.
pub struct Aes128Cfb8 {
    round_keys: [u8; 176],
    feedback: [u8; 16],
}

impl Aes128Cfb8 {
    /// CFB8 state with `key` and initial `iv`; Minecraft passes the shared
    /// secret for both.
    pub fn new(key: &[u8; 16], iv: &[u8; 16]) -> Self {
        Aes128Cfb8 {
            round_keys: aes128_expand_key(key),
            feedback: *iv,
        }
    }

    /// Encrypt in place; each produced byte feeds back for the next keystream
    /// byte.
    pub fn encrypt(&mut self, data: &mut [u8]) {
        for byte in data.iter_mut() {
            let keystream = aes128_encrypt_block_rk(&self.round_keys, &self.feedback);
            let ct = *byte ^ keystream[0];
            self.feedback.copy_within(1.., 0);
            self.feedback[15] = ct;
            *byte = ct;
        }
    }

    /// Decrypt in place; the *input* byte feeds back, not the recovered
    /// plaintext.
    pub fn decrypt(&mut self, data: &mut [u8]) {
        for byte in data.iter_mut() {
            let keystream = aes128_encrypt_block_rk(&self.round_keys, &self.feedback);
            let ct = *byte;
            *byte = ct ^ keystream[0];
            self.feedback.copy_within(1.., 0);
            self.feedback[15] = ct;
        }
    }
}

// ==================== minimal unsigned bignum (u32 limbs) ===================
//
// RSA needs modular exponentiation on ~1024-bit operands. This is a
// deliberately small magnitude type — compare, add, subtract, bitwise modular
// multiply and exponentiate — sufficient for PKCS#1 v1.5 work and nothing
// more. Values are normalized: no trailing zero limbs, `[]` is zero.

/// Non-negative arbitrary-precision integer, little-endian `u32` limbs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BigUint {
    limbs: Vec<u32>,
}

impl BigUint {
    /// The additive identity.
    pub fn zero() -> Self {
        Self::default()
    }

    /// Big-endian magnitude bytes -> value.
    pub fn from_be_bytes(bytes: &[u8]) -> Self {
        let mut limbs = Vec::with_capacity(bytes.len().div_ceil(4));
        let mut i = bytes.len();
        while i > 0 {
            let start = i.saturating_sub(4);
            let mut limb = 0u32;
            for &b in &bytes[start..i] {
                limb = (limb << 8) | u32::from(b);
            }
            limbs.push(limb);
            i = start;
        }
        let mut v = Self { limbs };
        v.normalize();
        v
    }

    /// Minimal-length big-endian magnitude (zero encodes as a single 0x00).
    pub fn to_be_bytes(&self) -> Vec<u8> {
        if self.limbs.is_empty() {
            return vec![0];
        }
        let mut out = Vec::with_capacity(self.limbs.len() * 4);
        for limb in self.limbs.iter().rev() {
            out.extend_from_slice(&limb.to_be_bytes());
        }
        let first = out.iter().position(|&b| b != 0).unwrap_or(out.len() - 1);
        out.drain(..first);
        out
    }

    /// Big-endian magnitude left-padded with zeros to exactly `len` bytes.
    pub fn to_be_bytes_padded(&self, len: usize) -> Vec<u8> {
        let raw = self.to_be_bytes();
        debug_assert!(raw.len() <= len);
        let mut out = vec![0u8; len.saturating_sub(raw.len())];
        out.extend_from_slice(&raw);
        out
    }

    /// Position of the most significant set bit + 1; zero has `bit_len == 0`.
    pub fn bit_len(&self) -> usize {
        self.limbs.last().map_or(0, |top| {
            (self.limbs.len() - 1) * 32 + (32 - top.leading_zeros() as usize)
        })
    }

    /// `true` for the value zero.
    pub fn is_zero(&self) -> bool {
        self.limbs.is_empty()
    }

    /// `true` when the least significant bit is clear (zero is even).
    pub fn is_even(&self) -> bool {
        self.limbs.first().is_none_or(|l| l & 1 == 0)
    }

    /// Bit `i` counting from the least significant bit.
    pub fn bit(&self, i: usize) -> bool {
        self.limbs
            .get(i / 32)
            .is_some_and(|l| (l >> (i % 32)) & 1 == 1)
    }

    fn normalize(&mut self) {
        while self.limbs.last() == Some(&0) {
            self.limbs.pop();
        }
    }

    /// `self * 2`, then `self |= bit0`.
    fn shl1_with(&mut self, bit0: bool) {
        let mut carry = u32::from(bit0);
        for l in self.limbs.iter_mut() {
            let next = *l >> 31;
            *l = (*l << 1) | carry;
            carry = next;
        }
        if carry != 0 {
            self.limbs.push(carry);
        }
    }

    /// `self + other` (unbounded).
    fn add(&self, other: &Self) -> Self {
        let (long, short) = if self.limbs.len() >= other.limbs.len() {
            (&self.limbs, &other.limbs)
        } else {
            (&other.limbs, &self.limbs)
        };
        let mut out = Vec::with_capacity(long.len() + 1);
        let mut carry = 0u64;
        for (i, &l) in long.iter().enumerate() {
            let s = u64::from(l) + u64::from(short.get(i).copied().unwrap_or(0)) + carry;
            out.push(s as u32);
            carry = s >> 32;
        }
        if carry != 0 {
            out.push(carry as u32);
        }
        let mut v = Self { limbs: out };
        v.normalize();
        v
    }

    /// `self - other`; requires `self >= other`.
    fn sub(&self, other: &Self) -> Self {
        debug_assert!(self >= other);
        let mut out = self.limbs.clone();
        let mut borrow = 0i64;
        for (i, slot) in out.iter_mut().enumerate() {
            let d = i64::from(*slot) - i64::from(other.limbs.get(i).copied().unwrap_or(0)) - borrow;
            if d < 0 {
                *slot = (d + (1i64 << 32)) as u32;
                borrow = 1;
            } else {
                *slot = d as u32;
                borrow = 0;
            }
        }
        debug_assert_eq!(borrow, 0);
        let mut v = Self { limbs: out };
        v.normalize();
        v
    }

    /// `self mod modulus` — bitwise long division, MSB first.
    pub fn rem(&self, modulus: &Self) -> Self {
        debug_assert!(!modulus.is_zero());
        let mut r = Self::zero();
        for i in (0..self.bit_len()).rev() {
            r.shl1_with(self.bit(i));
            if r >= *modulus {
                r = r.sub(modulus);
            }
        }
        r
    }

    /// `a + b mod m`; both inputs must already be `< m` (their sum is `< 2m`,
    /// so one conditional subtraction reduces it).
    fn add_mod(&self, other: &Self, modulus: &Self) -> Self {
        let sum = self.add(other);
        if sum >= *modulus {
            sum.sub(modulus)
        } else {
            sum
        }
    }

    /// `self * other mod modulus` — shift-and-add over `other`'s bits.
    pub fn mul_mod(&self, other: &Self, modulus: &Self) -> Self {
        debug_assert!(!modulus.is_zero());
        let mut result = Self::zero();
        let mut addend = self.rem(modulus);
        for i in 0..other.bit_len() {
            if other.bit(i) {
                result = result.add_mod(&addend, modulus);
            }
            addend = addend.add_mod(&addend, modulus);
        }
        result
    }

    /// `self^exponent mod modulus` — square-and-multiply, LSB first.
    pub fn mod_pow(&self, exponent: &Self, modulus: &Self) -> Self {
        debug_assert!(!modulus.is_zero());
        if modulus.limbs == [1] {
            return Self::zero();
        }
        let mut result = Self { limbs: vec![1] };
        let mut base = self.rem(modulus);
        for i in 0..exponent.bit_len() {
            if exponent.bit(i) {
                result = result.mul_mod(&base, modulus);
            }
            base = base.mul_mod(&base, modulus);
        }
        result
    }
}

impl Ord for BigUint {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        self.bit_len().cmp(&other.bit_len()).then_with(|| {
            for i in (0..self.limbs.len()).rev() {
                match self.limbs[i].cmp(&other.limbs[i]) {
                    Ordering::Equal => continue,
                    ord => return ord,
                }
            }
            Ordering::Equal
        })
    }
}

impl PartialOrd for BigUint {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

// =============== DER SubjectPublicKeyInfo -> RSA public key =================
//
// The Encryption Request carries the server's RSA key as X.509
// `SubjectPublicKeyInfo` DER:
//   SEQUENCE { SEQUENCE { OID rsaEncryption, NULL },
//              BIT STRING { SEQUENCE { INTEGER n, INTEGER e } } }
// Parsing is strict DER: every length must be exact, the algorithm OID must be
// `1.2.840.113549.1.1.1` with a NULL parameter, and the modulus/exponent are
// validated before any use.

/// `1.2.840.113549.1.1.1` (rsaEncryption) OID content bytes.
const RSA_ENCRYPTION_OID: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
/// Bound on the SPKI blob the wire may carry — RSA-1024 SPKI is 162 bytes;
/// generous headroom for larger keys without trusting the declared length.
const MAX_SPKI_DER: usize = 8192;
/// Minimum RSA modulus the client will encrypt to: vanilla uses exactly 1024
/// bits; anything smaller is rejected outright.
const MIN_RSA_MODULUS_BITS: usize = 1024;

/// A validated RSA public key parsed from SPKI DER.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RsaPublicKey {
    /// Modulus `n`.
    pub n: BigUint,
    /// Public exponent `e`.
    pub e: BigUint,
    /// RSA block size in bytes (`n`'s minimal byte length).
    pub key_bytes: usize,
}

/// Read one DER TLV with an expected tag; returns the content slice.
fn der_tlv<'a>(data: &'a [u8], pos: &mut usize, expect_tag: u8) -> Result<&'a [u8], String> {
    let bad = |m: &str| format!("malformed DER: {m}");
    if *pos >= data.len() {
        return Err(bad("truncated tag"));
    }
    let tag = data[*pos];
    *pos += 1;
    if tag != expect_tag {
        return Err(bad(&format!("tag {tag:#04x} != expected {expect_tag:#04x}")));
    }
    if *pos >= data.len() {
        return Err(bad("truncated length"));
    }
    let first = data[*pos];
    *pos += 1;
    let len = if first & 0x80 == 0 {
        usize::from(first)
    } else {
        let n = usize::from(first & 0x7f);
        // 0x80 is the forbidden indefinite form; >4 bytes is not plausible here.
        if n == 0 || n > 4 || *pos + n > data.len() {
            return Err(bad("invalid long-form length"));
        }
        let mut l = 0usize;
        for &b in &data[*pos..*pos + n] {
            l = (l << 8) | usize::from(b);
        }
        *pos += n;
        l
    };
    if len > data.len() - *pos {
        return Err(bad("truncated value"));
    }
    let value = &data[*pos..*pos + len];
    *pos += len;
    Ok(value)
}

/// Decode a canonical DER INTEGER content slice to a magnitude: at most one
/// `0x00` pad byte, only when the next byte's top bit is set; a set top bit on
/// the first byte (negative) or an empty content rejects.
fn der_integer_magnitude(raw: &[u8]) -> Result<&[u8], String> {
    let bad = "malformed DER: invalid INTEGER";
    match raw {
        [] => Err(bad.to_string()),
        [pad, rest @ ..] if *pad == 0 => {
            if rest.is_empty() || rest[0] & 0x80 == 0 {
                Err(bad.to_string())
            } else {
                Ok(rest)
            }
        }
        [first, ..] if first & 0x80 != 0 => Err(bad.to_string()),
        _ => Ok(raw),
    }
}

/// Write a DER length octet sequence for `len`.
fn der_write_len(out: &mut Vec<u8>, len: usize) {
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = len.to_be_bytes();
        let first = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len() - 1);
        let significant = &bytes[first..];
        out.push(0x80 | significant.len() as u8);
        out.extend_from_slice(significant);
    }
}

/// Assemble one DER TLV.
fn der_wrap(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len() + 8);
    out.push(tag);
    der_write_len(&mut out, content.len());
    out.extend_from_slice(content);
    out
}

/// DER INTEGER content for a big-endian magnitude (adds the `0x00` pad when
/// the top bit is set).
fn der_integer_encode(magnitude_be: &[u8]) -> Vec<u8> {
    let first = magnitude_be
        .iter()
        .position(|&b| b != 0)
        .unwrap_or(magnitude_be.len());
    let mag = if first >= magnitude_be.len() {
        &[0u8][..]
    } else {
        &magnitude_be[first..]
    };
    let mut content = Vec::with_capacity(mag.len() + 1);
    if mag[0] & 0x80 != 0 {
        content.push(0);
    }
    content.extend_from_slice(mag);
    der_wrap(0x02, &content)
}

/// Encode `SEQUENCE { SEQUENCE { OID rsaEncryption, NULL }, BIT STRING {
/// SEQUENCE { INTEGER n, INTEGER e } } }` — the X.509 SPKI form Java emits and
/// [`parse_rsa_public_key_der`] accepts. `modulus_be`/`exponent_be` are
/// big-endian magnitudes.
pub fn rsa_public_key_to_spki_der(modulus_be: &[u8], exponent_be: &[u8]) -> Vec<u8> {
    let mut pk = der_integer_encode(modulus_be);
    pk.extend_from_slice(&der_integer_encode(exponent_be));
    let pk_seq = der_wrap(0x30, &pk);
    let mut bit_string_content = vec![0u8]; // 0 unused bits
    bit_string_content.extend_from_slice(&pk_seq);
    let bit_string = der_wrap(0x03, &bit_string_content);
    let mut alg = der_wrap(0x06, RSA_ENCRYPTION_OID);
    alg.extend_from_slice(&der_wrap(0x05, &[]));
    let alg_seq = der_wrap(0x30, &alg);
    let mut spki = alg_seq;
    spki.extend_from_slice(&bit_string);
    der_wrap(0x30, &spki)
}

/// Strictly parse an SPKI DER blob into a validated [`RsaPublicKey`].
/// Fail-closed on: malformed DER, non-RSA OID, missing NULL parameter, stray
/// trailing bytes, negative/non-minimal INTEGERs, a modulus under
/// [`MIN_RSA_MODULUS_BITS`], an even modulus, or an exponent that is even,
/// under 3, or not below the modulus.
pub fn parse_rsa_public_key_der(der: &[u8]) -> Result<RsaPublicKey, String> {
    if der.len() > MAX_SPKI_DER {
        return Err("SPKI DER exceeds bound".to_string());
    }
    let mut pos = 0usize;
    let spki = der_tlv(der, &mut pos, 0x30)?;
    if pos != der.len() {
        return Err("malformed DER: trailing bytes after SPKI".to_string());
    }

    let mut p = 0usize;
    let alg = der_tlv(spki, &mut p, 0x30)?;
    {
        let mut q = 0usize;
        let oid = der_tlv(alg, &mut q, 0x06)?;
        if oid != RSA_ENCRYPTION_OID {
            return Err("SPKI algorithm is not rsaEncryption".to_string());
        }
        let params = der_tlv(alg, &mut q, 0x05)?;
        if !params.is_empty() {
            return Err("malformed DER: non-empty NULL parameters".to_string());
        }
        if q != alg.len() {
            return Err("malformed DER: trailing algorithm parameters".to_string());
        }
    }
    let bit_string = der_tlv(spki, &mut p, 0x03)?;
    if p != spki.len() {
        return Err("malformed DER: trailing SPKI fields".to_string());
    }
    if bit_string.first() != Some(&0) {
        return Err("malformed DER: unsupported BIT STRING".to_string());
    }
    let inner = &bit_string[1..];
    let mut q = 0usize;
    let seq = der_tlv(inner, &mut q, 0x30)?;
    if q != inner.len() {
        return Err("malformed DER: trailing BIT STRING bytes".to_string());
    }
    let mut r = 0usize;
    let n_raw = der_tlv(seq, &mut r, 0x02)?;
    let e_raw = der_tlv(seq, &mut r, 0x02)?;
    if r != seq.len() {
        return Err("malformed DER: trailing RSAPublicKey fields".to_string());
    }

    let n = BigUint::from_be_bytes(der_integer_magnitude(n_raw)?);
    let e = BigUint::from_be_bytes(der_integer_magnitude(e_raw)?);
    if n.bit_len() < MIN_RSA_MODULUS_BITS {
        return Err("RSA modulus below 1024 bits".to_string());
    }
    if n.is_even() {
        return Err("RSA modulus is even".to_string());
    }
    let three = BigUint { limbs: vec![3] };
    if e < three || e.is_even() || e >= n {
        return Err("invalid RSA public exponent".to_string());
    }
    let key_bytes = n.to_be_bytes().len();
    Ok(RsaPublicKey { n, e, key_bytes })
}

// =================== PKCS#1 v1.5 Type-2 encryption ==========================
//
// EM = 0x00 || 0x02 || PS || 0x00 || M, with PS >= 8 non-zero CSPRNG bytes;
// the integer is then raised to `e mod n` and emitted as exactly `k` bytes.

/// Build the PKCS#1 v1.5 Type-2 encoded message for `payload` in an RSA block
/// of `key_bytes` bytes, drawing the non-zero padding from `rng`.
pub fn rsa_pkcs1v15_pad(
    key_bytes: usize,
    payload: &[u8],
    rng: &mut dyn FnMut(&mut [u8]) -> Result<(), String>,
) -> Result<Vec<u8>, String> {
    if payload.len() + 11 > key_bytes {
        return Err("payload too large for RSA PKCS#1 v1.5 block".to_string());
    }
    let ps_len = key_bytes - payload.len() - 3;
    let mut em = Vec::with_capacity(key_bytes);
    em.push(0);
    em.push(2);
    // Bound the draw loop so a broken injected RNG cannot spin forever.
    for _draws in 0..4096 {
        if em.len() >= 2 + ps_len {
            break;
        }
        let mut b = [0u8; 1];
        rng(&mut b)?;
        if b[0] != 0 {
            em.push(b[0]);
        }
    }
    if em.len() != 2 + ps_len {
        return Err("CSPRNG starvation while padding".to_string());
    }
    em.push(0);
    em.extend_from_slice(payload);
    debug_assert_eq!(em.len(), key_bytes);
    Ok(em)
}

/// Strip a PKCS#1 v1.5 Type-2 encoded message; returns the payload slice.
/// Fail-closed on bad leaders, a missing separator, or padding under 8 bytes.
pub fn rsa_pkcs1v15_unpad(em: &[u8]) -> Result<&[u8], String> {
    let bad = "invalid PKCS#1 v1.5 block";
    if em.len() < 11 || em[0] != 0 || em[1] != 2 {
        return Err(bad.to_string());
    }
    let Some(sep) = em[2..].iter().position(|&b| b == 0).map(|i| i + 2) else {
        return Err(bad.to_string());
    };
    if sep - 2 < 8 {
        return Err(bad.to_string());
    }
    Ok(&em[sep + 1..])
}

/// RSA-encrypt `payload` with `key` using PKCS#1 v1.5 Type-2 padding; the
/// non-zero padding bytes come from `rng` (see [`fill_os_random`] and
/// [`OnlineAuthOptions::rng`]).
pub fn rsa_pkcs1v15_encrypt(
    key: &RsaPublicKey,
    payload: &[u8],
    rng: &mut dyn FnMut(&mut [u8]) -> Result<(), String>,
) -> Result<Vec<u8>, String> {
    let em = rsa_pkcs1v15_pad(key.key_bytes, payload, rng)?;
    let m = BigUint::from_be_bytes(&em);
    if m >= key.n {
        return Err("encoded message out of range for RSA modulus".to_string());
    }
    Ok(m.mod_pow(&key.e, &key.n).to_be_bytes_padded(key.key_bytes))
}

// ==================== Mojang session-server join =============================
//
// After computing the signed server hash, an online-mode client proves its
// session to Mojang before sending the Encryption Response:
//   POST https://sessionserver.mojang.com/session/minecraft/join
//   {"accessToken": <launcher token>, "selectedProfile": <undashed uuid>,
//    "serverId": <signed hash>}
// Success is an empty HTTP 204; anything else fails the login closed. The
// access token is passed only inside the request body — never in errors,
// Debug output, or logs.

/// The production Mojang join endpoint.
pub const MOJANG_SESSION_JOIN_URL: &str =
    "https://sessionserver.mojang.com/session/minecraft/join";
/// Default join request budget — inside the 15s connect timeout.
pub const DEFAULT_JOIN_TIMEOUT: Duration = Duration::from_secs(10);

/// One session-join proof. `access_token` is a secret: [`fmt::Debug`] redacts
/// it and it must never be copied into error strings.
pub struct SessionJoinRequest {
    /// The signed SHA-1 server hash (`serverId` field).
    pub server_hash: String,
    /// The launcher's Mojang access token — secret, never logged.
    pub access_token: String,
    /// The account UUID the client logged in with (sent undashed).
    pub selected_profile: [u8; 16],
}

impl SessionJoinRequest {
    /// Assemble the join proof for one login.
    pub fn new(
        server_hash: impl Into<String>,
        access_token: impl Into<String>,
        selected_profile: [u8; 16],
    ) -> Self {
        Self {
            server_hash: server_hash.into(),
            access_token: access_token.into(),
            selected_profile,
        }
    }

    /// `selectedProfile` in the undashed 32-hex form Mojang expects.
    pub fn profile_hex(&self) -> String {
        hex(&self.selected_profile)
    }
}

impl fmt::Debug for SessionJoinRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionJoinRequest")
            .field("server_hash", &self.server_hash)
            .field("access_token", &"<REDACTED>")
            .field("selected_profile", &self.profile_hex())
            .finish()
    }
}

/// The pluggable half of online-mode auth: production POSTs to Mojang;
/// offline tests inject a deterministic mock.
pub trait SessionJoinClient: Send + Sync {
    /// `Ok(())` iff the session server accepted the join proof (HTTP 204).
    /// Errors must be redacted — never echo `request.access_token` or the
    /// shared-secret-derived material.
    fn session_join(&self, request: &SessionJoinRequest) -> Result<(), String>;
}

/// The default [`SessionJoinClient`]: a JSON POST through the crate's
/// established `curl`-backed HTTPS transport (loopback `http://` is accepted
/// for deterministic tests via [`Self::with_endpoint`]).
pub struct MojangSessionJoinClient<T: ReportTransport = StdHttpReportTransport> {
    endpoint: String,
    transport: T,
    timeout: Duration,
}

impl Default for MojangSessionJoinClient {
    fn default() -> Self {
        Self::new()
    }
}

impl MojangSessionJoinClient<StdHttpReportTransport> {
    /// The production client against [`MOJANG_SESSION_JOIN_URL`].
    pub fn new() -> Self {
        Self {
            endpoint: MOJANG_SESSION_JOIN_URL.to_string(),
            transport: StdHttpReportTransport::new(),
            timeout: DEFAULT_JOIN_TIMEOUT,
        }
    }
}

impl<T: ReportTransport> MojangSessionJoinClient<T> {
    /// A client against a non-default endpoint/transport (tests, proxies).
    pub fn with_endpoint(endpoint: impl Into<String>, transport: T, timeout: Duration) -> Self {
        Self {
            endpoint: endpoint.into(),
            transport,
            timeout,
        }
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out
}

impl<T: ReportTransport + Send + Sync> SessionJoinClient for MojangSessionJoinClient<T> {
    fn session_join(&self, request: &SessionJoinRequest) -> Result<(), String> {
        let body = format!(
            "{{\"accessToken\":\"{}\",\"selectedProfile\":\"{}\",\"serverId\":\"{}\"}}",
            json_escape(&request.access_token),
            request.profile_hex(),
            json_escape(&request.server_hash)
        );
        match self.transport.post_json(&self.endpoint, &body, self.timeout) {
            Ok(resp) if resp.status == 204 => Ok(()),
            // Only the status code is reported: the response body may carry a
            // Mojang error document and is deliberately not echoed.
            Ok(resp) => Err(format!("session join rejected (HTTP {})", resp.status)),
            Err(e) => Err(format!("session join transport failed: {e}")),
        }
    }
}

// ============ Encryption Request/Response wire bodies + plan ================
//
// S2C Login `0x01` Encryption Request: serverId String, public key byte
// array, verify-token byte array, shouldAuthenticate bool. C2S `0x01`
// Encryption Response: two byte arrays (encrypted shared secret, encrypted
// verify token).

/// Bound on the `server_id` string (vanilla sends the empty string).
const MAX_SERVER_ID_CHARS: usize = 1024;
/// Bound on the DER public-key byte array on the wire.
const MAX_PUBLIC_KEY_BYTES: usize = MAX_SPKI_DER;
/// Bound on the `verify_token` byte array (vanilla sends 4 bytes).
const MAX_VERIFY_TOKEN_BYTES: usize = 1024;

/// Decoded `0x01 Encryption Request` fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncryptionRequestFields {
    /// Vanilla sends the empty string.
    pub server_id: String,
    /// X.509 SPKI DER encoding of the server's RSA public key.
    pub public_key_der: Vec<u8>,
    /// Challenge the client must echo back encrypted.
    pub verify_token: Vec<u8>,
    /// When true, the client must have completed the Mojang session join
    /// before answering.
    pub should_authenticate: bool,
}

/// Decode an Encryption Request body, fail-closed on truncation, overlong
/// fields, or trailing bytes.
pub fn decode_encryption_request_body(body: &[u8]) -> Result<EncryptionRequestFields, String> {
    let mut r = megu_proto::WireReader::new(body);
    let map_err = |e: megu_proto::ProtoError| format!("malformed encryption request: {e}");
    let fields = EncryptionRequestFields {
        server_id: r.read_string(MAX_SERVER_ID_CHARS).map_err(map_err)?,
        public_key_der: r
            .read_bytes(MAX_PUBLIC_KEY_BYTES)
            .map_err(map_err)?
            .to_vec(),
        verify_token: r
            .read_bytes(MAX_VERIFY_TOKEN_BYTES)
            .map_err(map_err)?
            .to_vec(),
        should_authenticate: r.read_bool().map_err(map_err)?,
    };
    if r.remaining() != 0 {
        return Err("malformed encryption request: trailing bytes".to_string());
    }
    Ok(fields)
}

/// Encode the `0x01 Encryption Response` body: two VarInt-length byte arrays.
pub fn encode_encryption_response_body(
    encrypted_shared_secret: &[u8],
    encrypted_verify_token: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        10 + encrypted_shared_secret.len() + encrypted_verify_token.len(),
    );
    let mut w = megu_proto::WireWriter::from_vec(std::mem::take(&mut out));
    w.write_bytes(encrypted_shared_secret);
    w.write_bytes(encrypted_verify_token);
    w.into_vec()
}

/// Everything [`EncryptionResponsePlan`] needs to finish the handshake: the
/// response body to send, then the shared secret that arms both ciphers.
pub struct EncryptionResponsePlan {
    /// The negotiated 16-byte shared secret — secret, never logged.
    pub shared_secret: [u8; 16],
    /// Ready-to-send `0x01 Encryption Response` body.
    pub response_body: Vec<u8>,
}

impl fmt::Debug for EncryptionResponsePlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncryptionResponsePlan")
            .field("shared_secret", &"<REDACTED>")
            .field("response_body_len", &self.response_body.len())
            .finish()
    }
}

/// Work one Encryption Request into a response plan: draw the shared secret,
/// compute the Mojang server hash, perform the session join when
/// `should_authenticate` (fails closed without a usable `access_token`), and
/// RSA-encrypt both fields. The caller then sends `response_body` plaintext
/// and arms AES-128-CFB8 with `shared_secret` in both directions.
///
/// `access_token` and a real launcher `profile_uuid` are required only when
/// `should_authenticate` is set; the token and the produced secret never
/// appear in error strings.
pub fn plan_encryption_response(
    fields: &EncryptionRequestFields,
    access_token: Option<&str>,
    profile_uuid: Option<[u8; 16]>,
    session_join: &dyn SessionJoinClient,
    rng: &mut dyn FnMut(&mut [u8]) -> Result<(), String>,
) -> Result<EncryptionResponsePlan, String> {
    let key = parse_rsa_public_key_der(&fields.public_key_der)?;

    let mut shared_secret = [0u8; 16];
    rng(&mut shared_secret)?;
    let server_hash = mc_server_hash(&fields.server_id, &shared_secret, &fields.public_key_der);

    if fields.should_authenticate {
        let token = access_token
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                "server requires online-mode authentication but no Mojang access \
                 token was provided (--accessToken); cannot join"
                    .to_string()
            })?;
        let uuid = profile_uuid.ok_or_else(|| {
            "server requires online-mode authentication but no account UUID was \
             provided (--uuid); cannot join"
                .to_string()
        })?;
        session_join.session_join(&SessionJoinRequest::new(
            server_hash,
            token.to_string(),
            uuid,
        ))?;
    }

    let enc_secret = rsa_pkcs1v15_encrypt(&key, &shared_secret, rng)?;
    let enc_token = rsa_pkcs1v15_encrypt(&key, &fields.verify_token, rng)?;
    Ok(EncryptionResponsePlan {
        shared_secret,
        response_body: encode_encryption_response_body(&enc_secret, &enc_token),
    })
}

// ========================= connect-time options =============================

/// Injected CSPRNG used for shared-secret and RSA padding bytes; `None`
/// installs [`fill_os_random`].
pub type RngSource = Box<dyn FnMut(&mut [u8]) -> Result<(), String> + Send>;

/// Online-mode inputs [`crate::protocol::ProtocolClient::connect_resolved`]
/// consumes when a server sends an Encryption Request. `Default` is the
/// offline profile: no token, the Mojang session-join client, and the OS
/// CSPRNG.
#[derive(Default)]
pub struct OnlineAuthOptions {
    /// The launcher's `--accessToken`. Consumed only when the server's
    /// Encryption Request carries `should_authenticate`; never logged.
    pub access_token: Option<String>,
    /// The launcher's real account `--uuid`. `None` when the client runs
    /// without one (the offline-derived login UUID is never substituted —
    /// Mojang only accepts the token's own `selectedProfile`).
    pub profile_uuid: Option<[u8; 16]>,
    /// Session-join override; `None` installs [`MojangSessionJoinClient`].
    pub session_join: Option<Arc<dyn SessionJoinClient>>,
    /// CSPRNG override for deterministic tests; `None` reads
    /// [`fill_os_random`].
    pub rng: Option<RngSource>,
}

impl fmt::Debug for OnlineAuthOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OnlineAuthOptions")
            .field("access_token", &self.access_token.as_ref().map(|_| "<REDACTED>"))
            .field("profile_uuid", &self.profile_uuid.map(|u| hex(&u)))
            .field("session_join", &self.session_join.is_some())
            .field("rng", &self.rng.is_some())
            .finish()
    }
}
