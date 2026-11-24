// SPDX-License-Identifier: LGPL-3.0-or-later
#![deny(unsafe_code)]
//! `FRL3` zero-trust end-to-end encryption for the friend relay: pure-Rust,
//! zero-dependency RFC 7748 X25519, RFC 8439 ChaCha20-Poly1305 AEAD and
//! RFC 5869 HKDF-SHA256 (built on [`crate::frl_auth::hmac_sha256`]) forming a
//! directional record layer that lives *inside*
//! [`crate::friend_relay::FriendPacket::StreamData`] payloads.
//!
//! ## Why this exists
//!
//! `FRL2` admission control (`GuestJoin` proofs, invite expiry, stream
//! isolation) is enforced by the relay — which means the relay is inside the
//! trust boundary. `FRL3` moves confidentiality and integrity *outside* it:
//! every byte the relay forwards between host and guest is an AEAD
//! ciphertext under keys only the two endpoints share, so a compromised or
//! malicious relay — or any active MITM on the path — cannot read, splice,
//! reorder, replay or bit-flip a single byte of game traffic.
//!
//! ## Two handshake shapes
//!
//! * **`FRL3-PSK`** ([`Frl3GuestHandshake`] + [`host_accept`]): an ephemeral
//!   X25519 exchange bound to the 40-bit `MEGU-XXXX-XXXX` invite code. The
//!   code is the PSK: `Hello.guest_mac` proves the guest holds it, and
//!   `Ack.host_confirm` proves the host holds it *and* the ephemeral secret,
//!   so a relay that substitutes keys fails closed.
//! * **`FRL3-FRIEND`** ([`Frl3FriendGuestHandshake`] +
//!   [`friend_host_accept`]): Noise-IK / WireGuard-style mutual
//!   authentication between long-term [`Frl3Identity`] static keys — no
//!   brute-forceable PSK at all. The host gates admission on a
//!   [`FriendAllowlist`] of trusted peer static keys (with optional
//!   128-bit [`Frl3GuestToken`]s for one-off guests), the guest pins the
//!   host's static public key it paired with out-of-band, and the session
//!   keys mix **four** Diffie-Hellman results
//!   `DH(e_g, e_h) || DH(e_g, s_h) || DH(s_g, e_h) || DH(s_g, s_h)` —
//!   ephemeral-ephemeral, ephemeral-static, static-ephemeral and
//!   static-static — so impersonating either side requires stealing a
//!   256-bit private key.
//!
//! Both end with [`sas_code`]: a six-digit code derived from the shared
//! transcript hash that two players can compare visually when pairing.
//!
//! ## Record layer
//!
//! [`Frl3Cipher`] frames each record as `u32be(ct_len) | ciphertext |
//! tag(16)`, capped by [`MAX_E2EE_RECORD_BYTES`] (checked before any
//! allocation). The nonce is `[dir, 0,0,0 | seq_be(8)]` and the AAD is
//! `b"FRL3" | dir | stream_id_be(4) | seq_be(8)`, so every chunk is bound to
//! its stream, direction and strict-monotonic sequence number — replayed or
//! reordered records cannot verify, and the counter fails closed rather
//! than wrapping.

use std::collections::BTreeMap;
use std::fmt;

use crate::frl_auth;
use crate::friend_relay::InviteCode;

/// Hard cap on one `FRL3` record's ciphertext length (the `u32be` prefix is
/// validated against this bound before a single byte is copied). 64 KiB
/// keeps a record comfortably inside one `FRL2` `StreamData` payload.
pub const MAX_E2EE_RECORD_BYTES: usize = 64 * 1024;

/// Every failure mode of the `FRL3` layer. All of them are fail-closed: the
/// caller learns *that* the peer/frame was rejected, never a guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum E2eeError {
    /// An AEAD tag, a handshake MAC or a key-confirmation proof did not
    /// verify in constant time — the bytes were tampered with, replayed,
    /// reordered or forged.
    AuthenticationFailed,
    /// An X25519 scalar multiplication produced the all-zero (low-order /
    /// contributory) shared secret — the peer sent a degenerate public key.
    ContributorySecret,
    /// A handshake message or record frame did not parse: bad magic, bad
    /// tag, wrong length or a forged `ct_len` prefix.
    Malformed,
    /// A record's declared ciphertext length exceeds
    /// [`MAX_E2EE_RECORD_BYTES`].
    RecordTooLarge,
    /// The record sequence counter reached `u64::MAX`; the session must be
    /// re-established — it can never wrap a nonce.
    CounterExhausted,
    /// The peer's static public key is not in the host's
    /// [`FriendAllowlist`], or a required guest token did not match.
    UnknownPeer,
    /// A `megu-id:` friend-id string failed to parse or its checksum
    /// mismatched.
    BadFriendId,
}

impl fmt::Display for E2eeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            E2eeError::AuthenticationFailed => "FRL3 authentication failed",
            E2eeError::ContributorySecret => "FRL3 peer sent a low-order public key",
            E2eeError::Malformed => "malformed FRL3 message or record",
            E2eeError::RecordTooLarge => "FRL3 record exceeds the size cap",
            E2eeError::CounterExhausted => "FRL3 sequence counter exhausted",
            E2eeError::UnknownPeer => "FRL3 peer is not on the allowlist",
            E2eeError::BadFriendId => "invalid megu-id friend identifier",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for E2eeError {}

// ===========================================================================
// Field arithmetic over GF(2^255 - 19), radix 2^51 (five u64 limbs).
//
// Every operation ends with a carry reduction, so all limbs stay below
// 2^51 + epsilon: `fe_sub`'s 2p bias can then never underflow, and `fe_mul`'s
// u128 accumulators have ~20 bits of headroom. Nothing branches on secret
// data — the conditional swap is a mask, the ladder loop is fixed-length.
// ===========================================================================

/// A field element modulo p = 2^255 - 19 as five 51-bit limbs, little-endian.
type Fe = [u64; 5];

const MASK51: u64 = (1 << 51) - 1;

/// Two-and-a-bit carry passes that leave every limb below 2^51 + epsilon.
fn fe_carry(h: &mut Fe) {
    let mut c = h[0] >> 51;
    h[0] &= MASK51;
    h[1] += c;
    c = h[1] >> 51;
    h[1] &= MASK51;
    h[2] += c;
    c = h[2] >> 51;
    h[2] &= MASK51;
    h[3] += c;
    c = h[3] >> 51;
    h[3] &= MASK51;
    h[4] += c;
    c = h[4] >> 51;
    h[4] &= MASK51;
    h[0] += c * 19;
    c = h[0] >> 51;
    h[0] &= MASK51;
    h[1] += c;
}

fn fe_add(a: &Fe, b: &Fe) -> Fe {
    let mut h = [0u64; 5];
    for i in 0..5 {
        h[i] = a[i] + b[i];
    }
    fe_carry(&mut h);
    h
}

fn fe_sub(a: &Fe, b: &Fe) -> Fe {
    // Bias by 2p in redundant limb form so the result stays non-negative:
    // 2*(2^51-19) on limb 0, 2*(2^51-1) on the rest.
    let mut h = [
        (a[0] + 0xF_FFFF_FFFF_FFDA) - b[0],
        (a[1] + 0xF_FFFF_FFFF_FFFE) - b[1],
        (a[2] + 0xF_FFFF_FFFF_FFFE) - b[2],
        (a[3] + 0xF_FFFF_FFFF_FFFE) - b[3],
        (a[4] + 0xF_FFFF_FFFF_FFFE) - b[4],
    ];
    fe_carry(&mut h);
    h
}

/// Schoolbook 5x5 product with the 19-fold wraparound applied inline (each
/// high term multiplied by 19 before accumulating), then carried.
fn fe_mul(a: &Fe, b: &Fe) -> Fe {
    let b1 = u128::from(b[1]) * 19;
    let b2 = u128::from(b[2]) * 19;
    let b3 = u128::from(b[3]) * 19;
    let b4 = u128::from(b[4]) * 19;
    let a0 = u128::from(a[0]);
    let a1 = u128::from(a[1]);
    let a2 = u128::from(a[2]);
    let a3 = u128::from(a[3]);
    let a4 = u128::from(a[4]);
    let mut h = [
        a0 * u128::from(b[0]) + a1 * b4 + a2 * b3 + a3 * b2 + a4 * b1,
        a0 * u128::from(b[1]) + a1 * u128::from(b[0]) + a2 * b4 + a3 * b3 + a4 * b2,
        a0 * u128::from(b[2]) + a1 * u128::from(b[1]) + a2 * u128::from(b[0]) + a3 * b4 + a4 * b3,
        a0 * u128::from(b[3]) + a1 * u128::from(b[2]) + a2 * u128::from(b[1]) + a3 * u128::from(b[0])
            + a4 * b4,
        a0 * u128::from(b[4]) + a1 * u128::from(b[3]) + a2 * u128::from(b[2]) + a3 * u128::from(b[1])
            + a4 * u128::from(b[0]),
    ];
    let mut c = h[0] >> 51;
    h[0] &= u128::from(MASK51);
    h[1] += c;
    c = h[1] >> 51;
    h[1] &= u128::from(MASK51);
    h[2] += c;
    c = h[2] >> 51;
    h[2] &= u128::from(MASK51);
    h[3] += c;
    c = h[3] >> 51;
    h[3] &= u128::from(MASK51);
    h[4] += c;
    c = h[4] >> 51;
    h[4] &= u128::from(MASK51);
    h[0] += c * 19;
    c = h[0] >> 51;
    h[0] &= u128::from(MASK51);
    h[1] += c;
    [
        h[0] as u64,
        h[1] as u64,
        h[2] as u64,
        h[3] as u64,
        h[4] as u64,
    ]
}

fn fe_sq(a: &Fe) -> Fe {
    fe_mul(a, a)
}

/// Multiplication by the small ladder constant a24 = 121665 = (486662-2)/4.
fn fe_mul_a24(a: &Fe) -> Fe {
    let mut h = [0u128; 5];
    for i in 0..5 {
        h[i] = u128::from(a[i]) * 121665;
    }
    let mut c = h[0] >> 51;
    h[0] &= u128::from(MASK51);
    h[1] += c;
    c = h[1] >> 51;
    h[1] &= u128::from(MASK51);
    h[2] += c;
    c = h[2] >> 51;
    h[2] &= u128::from(MASK51);
    h[3] += c;
    c = h[3] >> 51;
    h[3] &= u128::from(MASK51);
    h[4] += c;
    c = h[4] >> 51;
    h[4] &= u128::from(MASK51);
    h[0] += c * 19;
    c = h[0] >> 51;
    h[0] &= u128::from(MASK51);
    h[1] += c;
    [
        h[0] as u64,
        h[1] as u64,
        h[2] as u64,
        h[3] as u64,
        h[4] as u64,
    ]
}

/// Constant-time conditional swap: when `swap` is 1, `a` and `b` trade
/// contents. Masked XOR — no branch, no data-dependent index.
fn fe_cswap(a: &mut Fe, b: &mut Fe, swap: u64) {
    let m = 0u64.wrapping_sub(swap);
    for i in 0..5 {
        let t = m & (a[i] ^ b[i]);
        a[i] ^= t;
        b[i] ^= t;
    }
}

/// h^(p-2) = h^(2^255 - 21): the standard fixed addition chain.
fn fe_invert(x: &Fe) -> Fe {
    let t0 = fe_sq(x); // x^2
    let mut t1 = fe_sq(&t0); // x^4
    t1 = fe_sq(&t1); // x^8
    t1 = fe_mul(x, &t1); // x^9
    let t0 = fe_mul(&t0, &t1); // x^11
    let t2 = fe_sq(&t0); // x^22
    let t1 = fe_mul(&t1, &t2); // x^31 = x^(2^5-1)
    let mut t2 = fe_sq(&t1);
    for _ in 1..5 {
        t2 = fe_sq(&t2);
    }
    let z10 = fe_mul(&t1, &t2); // x^(2^10-1)
    let mut t3 = fe_sq(&z10);
    for _ in 1..10 {
        t3 = fe_sq(&t3);
    }
    let z20 = fe_mul(&z10, &t3); // x^(2^20-1)
    let mut t2 = fe_sq(&z20);
    for _ in 1..20 {
        t2 = fe_sq(&t2);
    }
    let z40 = fe_mul(&z20, &t2); // x^(2^40-1)
    let mut t3 = fe_sq(&z40);
    for _ in 1..10 {
        t3 = fe_sq(&t3);
    }
    let z50 = fe_mul(&z10, &t3); // x^(2^50-1)
    let mut t4 = fe_sq(&z50);
    for _ in 1..50 {
        t4 = fe_sq(&t4);
    }
    let z100 = fe_mul(&z50, &t4); // x^(2^100-1)
    let mut t5 = fe_sq(&z100);
    for _ in 1..100 {
        t5 = fe_sq(&t5);
    }
    let z200 = fe_mul(&z100, &t5); // x^(2^200-1)
    let mut t4 = fe_sq(&z200);
    for _ in 1..50 {
        t4 = fe_sq(&t4);
    }
    let z250 = fe_mul(&z50, &t4); // x^(2^250-1)
    let mut t4 = fe_sq(&z250);
    for _ in 1..5 {
        t4 = fe_sq(&t4);
    }
    // 2^255 - 32, then one multiply by x^11 = t0 gives x^(2^255 - 21).
    fe_mul(&t0, &t4)
}

/// Load 32 little-endian bytes into five 51-bit limbs. The caller masks the
/// top bit for X25519 u-coordinates.
fn fe_from_bytes(b: &[u8; 32]) -> Fe {
    let u0 = u64::from_le_bytes(b[0..8].try_into().unwrap());
    let u1 = u64::from_le_bytes(b[8..16].try_into().unwrap());
    let u2 = u64::from_le_bytes(b[16..24].try_into().unwrap());
    let u3 = u64::from_le_bytes(b[24..32].try_into().unwrap());
    [
        u0 & MASK51,
        ((u0 >> 51) | (u1 << 13)) & MASK51,
        ((u1 >> 38) | (u2 << 26)) & MASK51,
        ((u2 >> 25) | (u3 << 39)) & MASK51,
        (u3 >> 12) & MASK51,
    ]
}

/// Canonical 32-byte little-endian encoding: fully reduce mod p, then pack.
fn fe_to_bytes(x: &Fe) -> [u8; 32] {
    let mut h = *x;
    fe_carry(&mut h);
    // q = 1 iff h + 19 overflows past 2^255, i.e. h >= p: fold it back.
    let mut q = (h[0] + 19) >> 51;
    q = (h[1] + q) >> 51;
    q = (h[2] + q) >> 51;
    q = (h[3] + q) >> 51;
    q = (h[4] + q) >> 51;
    h[0] += 19 * q;
    let mut c = h[0] >> 51;
    h[0] &= MASK51;
    h[1] += c;
    c = h[1] >> 51;
    h[1] &= MASK51;
    h[2] += c;
    c = h[2] >> 51;
    h[2] &= MASK51;
    h[3] += c;
    c = h[3] >> 51;
    h[3] &= MASK51;
    h[4] += c;
    h[4] &= MASK51;
    let u0 = h[0] | (h[1] << 51);
    let u1 = (h[1] >> 13) | (h[2] << 38);
    let u2 = (h[2] >> 26) | (h[3] << 25);
    let u3 = (h[3] >> 39) | (h[4] << 12);
    let mut out = [0u8; 32];
    out[0..8].copy_from_slice(&u0.to_le_bytes());
    out[8..16].copy_from_slice(&u1.to_le_bytes());
    out[16..24].copy_from_slice(&u2.to_le_bytes());
    out[24..32].copy_from_slice(&u3.to_le_bytes());
    out
}

/// RFC 7748 X25519: scalar multiplication on the Montgomery form of
/// Curve25519. `scalar` is clamped per RFC 7748 section 5; `u_coord`'s top
/// bit is masked. Returns the 32-byte little-endian shared u-coordinate —
/// callers handling peer-supplied public keys **must** reject the all-zero
/// result ([`dh`] does this automatically).
pub fn x25519(scalar: &[u8; 32], u_coord: &[u8; 32]) -> [u8; 32] {
    let mut k = *scalar;
    k[0] &= 248;
    k[31] = (k[31] & 127) | 64;
    let mut ub = *u_coord;
    ub[31] &= 127;
    let x1 = fe_from_bytes(&ub);
    let mut x2: Fe = [1, 0, 0, 0, 0];
    let mut z2: Fe = [0; 5];
    let mut x3 = x1;
    let mut z3: Fe = [1, 0, 0, 0, 0];
    let mut swap = 0u64;
    for t in (0..255).rev() {
        let kt = u64::from((k[t >> 3] >> (t & 7)) & 1);
        swap ^= kt;
        fe_cswap(&mut x2, &mut x3, swap);
        fe_cswap(&mut z2, &mut z3, swap);
        swap = kt;
        let a = fe_add(&x2, &z2);
        let aa = fe_sq(&a);
        let b = fe_sub(&x2, &z2);
        let bb = fe_sq(&b);
        let e = fe_sub(&aa, &bb);
        let c = fe_add(&x3, &z3);
        let d = fe_sub(&x3, &z3);
        let da = fe_mul(&d, &a);
        let cb = fe_mul(&c, &b);
        x3 = fe_sq(&fe_add(&da, &cb));
        z3 = fe_mul(&x1, &fe_sq(&fe_sub(&da, &cb)));
        x2 = fe_mul(&aa, &bb);
        z2 = fe_mul(&e, &fe_add(&aa, &fe_mul_a24(&e)));
    }
    fe_cswap(&mut x2, &mut x3, swap);
    fe_cswap(&mut z2, &mut z3, swap);
    fe_to_bytes(&fe_mul(&x2, &fe_invert(&z2)))
}

/// The X25519 base-point multiplication: the public key for `secret`.
pub fn x25519_public(secret: &[u8; 32]) -> [u8; 32] {
    let mut base = [0u8; 32];
    base[0] = 9;
    x25519(secret, &base)
}

/// Peer-facing Diffie-Hellman: like [`x25519`] but fails closed on the
/// all-zero contributory/low-order result.
fn dh(secret: &[u8; 32], public: &[u8; 32]) -> Result<[u8; 32], E2eeError> {
    let shared = x25519(secret, public);
    if frl_auth::ct_eq(&shared, &[0u8; 32]) {
        return Err(E2eeError::ContributorySecret);
    }
    Ok(shared)
}

// ===========================================================================
// RFC 8439 ChaCha20 (IETF 96-bit nonce variant) + Poly1305 + AEAD.
// ===========================================================================

const CHACHA_CONST: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

fn chacha_qr(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

/// One 64-byte ChaCha20 keystream block.
pub(crate) fn chacha20_block(key: &[u8; 32], counter: u32, nonce: &[u8; 12]) -> [u8; 64] {
    let mut s = [0u32; 16];
    s[..4].copy_from_slice(&CHACHA_CONST);
    for i in 0..8 {
        s[4 + i] = u32::from_le_bytes(key[i * 4..i * 4 + 4].try_into().unwrap());
    }
    s[12] = counter;
    for i in 0..3 {
        s[13 + i] = u32::from_le_bytes(nonce[i * 4..i * 4 + 4].try_into().unwrap());
    }
    let mut w = s;
    for _ in 0..10 {
        chacha_qr(&mut w, 0, 4, 8, 12);
        chacha_qr(&mut w, 1, 5, 9, 13);
        chacha_qr(&mut w, 2, 6, 10, 14);
        chacha_qr(&mut w, 3, 7, 11, 15);
        chacha_qr(&mut w, 0, 5, 10, 15);
        chacha_qr(&mut w, 1, 6, 11, 12);
        chacha_qr(&mut w, 2, 7, 8, 13);
        chacha_qr(&mut w, 3, 4, 9, 14);
    }
    let mut out = [0u8; 64];
    for (i, v) in w.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.wrapping_add(s[i]).to_le_bytes());
    }
    out
}

fn chacha20_xor(key: &[u8; 32], counter: u32, nonce: &[u8; 12], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut ctr = counter;
    for chunk in data.chunks(64) {
        let ks = chacha20_block(key, ctr, nonce);
        ctr = ctr.wrapping_add(1);
        out.extend(chunk.iter().zip(ks.iter()).map(|(&b, &k)| b ^ k));
    }
    out
}

/// Poly1305 one-time authenticator (three 44-bit limbs, donna-64 layout).
fn poly1305_auth(msg: &[u8], key: &[u8; 32]) -> [u8; 16] {
    const M44: u64 = (1 << 44) - 1;
    let t0 = u64::from_le_bytes(key[0..8].try_into().unwrap());
    let t1 = u64::from_le_bytes(key[8..16].try_into().unwrap());
    let r0 = t0 & 0xffc_0fff_ffff;
    let r1 = ((t0 >> 44) | (t1 << 20)) & 0xfff_ffc0_ffff;
    let r2 = (t1 >> 24) & 0x000f_ffff_fc0f;
    // 2^132 = 4*2^130 ≡ 20 (mod p): the limb-3 wrap factor is 5<<2.
    let s1 = r1 * 20;
    let s2 = r2 * 20;
    let pad0 = u64::from_le_bytes(key[16..24].try_into().unwrap());
    let pad1 = u64::from_le_bytes(key[24..32].try_into().unwrap());
    let (mut h0, mut h1, mut h2) = (0u64, 0u64, 0u64);

    let mut fold = |m0: u64, m1: u64, hibit: u64| {
        h0 = h0.wrapping_add(m0 & 0xfff_ffff_ffff);
        h1 = h1.wrapping_add(((m0 >> 44) | (m1 << 20)) & 0xfff_ffff_ffff);
        h2 = h2.wrapping_add(((m1 >> 24) & 0x3ff_ffff_ffff) | hibit);
        let d0 = u128::from(h0) * u128::from(r0)
            + u128::from(h1) * u128::from(s2)
            + u128::from(h2) * u128::from(s1);
        let mut d1 = u128::from(h0) * u128::from(r1)
            + u128::from(h1) * u128::from(r0)
            + u128::from(h2) * u128::from(s2);
        let mut d2 = u128::from(h0) * u128::from(r2)
            + u128::from(h1) * u128::from(r1)
            + u128::from(h2) * u128::from(r0);
        let mut c = d0 >> 44;
        h0 = d0 as u64 & M44;
        d1 += c;
        c = d1 >> 44;
        h1 = d1 as u64 & M44;
        d2 += c;
        c = d2 >> 42;
        h2 = d2 as u64 & 0x3ff_ffff_ffff;
        h0 += c as u64 * 5;
        c = u128::from(h0) >> 44;
        h0 &= M44;
        h1 += c as u64;
    };

    let (blocks, rem) = msg.as_chunks::<16>();
    for block in blocks {
        let m0 = u64::from_le_bytes(block[0..8].try_into().unwrap());
        let m1 = u64::from_le_bytes(block[8..16].try_into().unwrap());
        fold(m0, m1, 1 << 40);
    }
    if !rem.is_empty() {
        let mut block = [0u8; 16];
        block[..rem.len()].copy_from_slice(rem);
        block[rem.len()] = 1;
        let m0 = u64::from_le_bytes(block[0..8].try_into().unwrap());
        let m1 = u64::from_le_bytes(block[8..16].try_into().unwrap());
        fold(m0, m1, 0);
    }

    // Full carry, then select h or h+5-p in constant time.
    let mut c = h1 >> 44;
    h1 &= M44;
    h2 += c;
    c = h2 >> 42;
    h2 &= 0x3ff_ffff_ffff;
    h0 += c * 5;
    c = h0 >> 44;
    h0 &= M44;
    h1 += c;
    let mut g0 = h0 + 5;
    c = g0 >> 44;
    g0 &= M44;
    let mut g1 = h1 + c;
    c = g1 >> 44;
    g1 &= M44;
    let g2 = h2.wrapping_add(c).wrapping_sub(1 << 42);
    let mask = (g2 >> 63).wrapping_sub(1);
    g0 &= mask;
    g1 &= mask;
    let g2 = g2 & mask;
    let nmask = !mask;
    let h0 = (h0 & nmask) | g0;
    let h1 = (h1 & nmask) | g1;
    let h2 = (h2 & nmask) | g2;

    // Fold to 128 bits and add the pad half of the one-time key.
    let lo = h0 | (h1 << 44);
    let hi = (h1 >> 20) | (h2 << 24);
    let lo = lo.wrapping_add(pad0);
    let hi = hi.wrapping_add(pad1).wrapping_add(u64::from(lo < pad0));
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&lo.to_le_bytes());
    out[8..].copy_from_slice(&hi.to_le_bytes());
    out
}

/// `aad | pad16 | ct | pad16 | le64(aad.len) | le64(ct.len)` per RFC 8439 §2.8.
fn aead_mac_data(aad: &[u8], ct: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(aad.len() + ct.len() + 32);
    m.extend_from_slice(aad);
    let aad_pad = (16 - aad.len() % 16) % 16;
    m.resize(m.len() + aad_pad, 0);
    m.extend_from_slice(ct);
    let ct_pad = (16 - ct.len() % 16) % 16;
    m.resize(m.len() + ct_pad, 0);
    m.extend_from_slice(&(aad.len() as u64).to_le_bytes());
    m.extend_from_slice(&(ct.len() as u64).to_le_bytes());
    m
}

/// RFC 8439 §2.8 AEAD seal: returns `ciphertext || tag(16)`. The Poly1305
/// one-time key is block 0 of the keystream; encryption runs from counter 1.
pub fn aead_seal(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let block0 = chacha20_block(key, 0, nonce);
    let mut otk = [0u8; 32];
    otk.copy_from_slice(&block0[..32]);
    let mut out = chacha20_xor(key, 1, nonce, plaintext);
    let tag = poly1305_auth(&aead_mac_data(aad, &out), &otk);
    out.extend_from_slice(&tag);
    out
}

/// RFC 8439 §2.8 AEAD open on `ciphertext || tag(16)`. The tag is verified
/// in constant time *before* a single plaintext byte is produced.
pub fn aead_open(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    ct_and_tag: &[u8],
) -> Result<Vec<u8>, E2eeError> {
    if ct_and_tag.len() < 16 {
        return Err(E2eeError::Malformed);
    }
    let (ct, tag) = ct_and_tag.split_at(ct_and_tag.len() - 16);
    let block0 = chacha20_block(key, 0, nonce);
    let mut otk = [0u8; 32];
    otk.copy_from_slice(&block0[..32]);
    let expected = poly1305_auth(&aead_mac_data(aad, ct), &otk);
    if !frl_auth::ct_eq(&expected, tag) {
        return Err(E2eeError::AuthenticationFailed);
    }
    Ok(chacha20_xor(key, 1, nonce, ct))
}

// ===========================================================================
// RFC 5869 HKDF-SHA256 over the shared HMAC implementation — zero new hash code.
// ===========================================================================

/// HKDF-Extract: `PRK = HMAC-SHA256(salt, ikm)` (empty salt = 32 zero bytes).
pub fn hkdf_extract(salt: &[u8], ikm: &[u8]) -> [u8; 32] {
    let zeros = [0u8; 32];
    let salt = if salt.is_empty() { &zeros[..] } else { salt };
    frl_auth::hmac_sha256(salt, ikm)
}

/// HKDF-Expand: `T(1) | T(2) | ...` truncated to `out_len` (clamped to
/// 255*32 = 8160 bytes, the RFC 5869 maximum).
pub fn hkdf_expand(prk: &[u8; 32], info: &[u8], out_len: usize) -> Vec<u8> {
    let n = out_len.min(255 * 32);
    let mut out = Vec::with_capacity(n);
    let mut t: Vec<u8> = Vec::new();
    let mut i = 1u8;
    while out.len() < n {
        let mut m = Vec::with_capacity(t.len() + info.len() + 1);
        m.extend_from_slice(&t);
        m.extend_from_slice(info);
        m.push(i);
        t = frl_auth::hmac_sha256(prk, &m).to_vec();
        out.extend_from_slice(&t);
        i = i.wrapping_add(1);
    }
    out.truncate(n);
    out
}

/// 32 bytes of OS entropy for ephemeral secrets: the platform CSPRNG via
/// [`crate::friend_relay`]'s `/dev/urandom`/`BCryptGenRandom` path, with the
/// same portable mixer (pid + ASLR + clock jitter) folded through SHA-256 as
/// the fallback for platforms where neither is available.
pub fn random_32_bytes() -> [u8; 32] {
    let mut out = [0u8; 32];
    if crate::friend_relay::os_random_bytes(&mut out) {
        return out;
    }
    let mut material = [0u8; 32];
    for chunk in material.chunks_mut(8) {
        chunk.copy_from_slice(&crate::friend_relay::draw_seed().to_ne_bytes());
    }
    frl_auth::sha256(&material)
}

// ===========================================================================
// Handshake wire messages — the first `StreamData` payloads on a stream.
// All start with b"FRL3" | tag so they cannot collide with record framing
// (a record's u32be length is always < 2^17, i.e. its first byte is 0x00).
// ===========================================================================

const FRL3_MAGIC: [u8; 4] = *b"FRL3";
const TAG_HELLO: u8 = 0x01;
const TAG_ACK: u8 = 0x02;
const TAG_FRIEND_HELLO: u8 = 0x03;
const TAG_FRIEND_ACK: u8 = 0x04;

/// Guest → host, `FRL3-PSK` step 1: the guest's ephemeral public key and an
/// invite-code-keyed MAC proving knowledge of the PSK over the bound fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frl3Hello {
    /// Fresh ephemeral X25519 public key.
    pub guest_eph_pk: [u8; 32],
    /// `HMAC-SHA256(invite_code, "FRL3/ghello" | stream_id | guest_name | guest_eph_pk)`.
    pub guest_mac: [u8; 32],
}

/// Host → guest, `FRL3-PSK` step 2: the host's ephemeral public key plus a
/// confirmation MAC under `k_h2g` proving the same invite code *and* a
/// matching ephemeral secret — the anti-MITM anchor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frl3Ack {
    /// Fresh ephemeral X25519 public key.
    pub host_eph_pk: [u8; 32],
    /// `HMAC-SHA256(k_h2g, "FRL3/hconfirm" | stream_id | guest_eph_pk | host_eph_pk)`.
    pub host_confirm: [u8; 32],
}

/// Guest → host, `FRL3-FRIEND` step 1: ephemeral + static keys, the optional
/// one-off guest token, and a proof computed under `DH(s_g, s_h)` — only a
/// peer holding the static secret can produce it, and it binds every field
/// so a relay cannot substitute any of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frl3FriendHello {
    /// Fresh ephemeral X25519 public key.
    pub guest_eph_pk: [u8; 32],
    /// Long-term static public key — the `megu-id:` the host allowlists.
    pub guest_static_pk: [u8; 32],
    /// 128-bit one-off guest token ([`Frl3GuestToken::NONE`] when unused).
    pub token: [u8; 16],
    /// `HMAC-SHA256(HKDF(s_h_pk, DH(s_g,s_h)), "FRL3/fproof" | stream_id |
    /// guest_name | guest_eph_pk | guest_static_pk | token)`.
    pub proof: [u8; 32],
}

/// Host → guest, `FRL3-FRIEND` step 2.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frl3FriendAck {
    /// Fresh ephemeral X25519 public key.
    pub host_eph_pk: [u8; 32],
    /// `HMAC-SHA256(k_h2g, "FRL3/fconfirm" | stream_id | guest_eph_pk |
    /// guest_static_pk | host_eph_pk)`.
    pub host_confirm: [u8; 32],
}

/// One decoded handshake message (records never decode through here — a
/// payload not starting with `b"FRL3"` is a record for [`Frl3Cipher::open`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frl3Message {
    /// `FRL3-PSK` guest hello.
    Hello(Frl3Hello),
    /// `FRL3-PSK` host ack.
    Ack(Frl3Ack),
    /// `FRL3-FRIEND` guest hello.
    FriendHello(Frl3FriendHello),
    /// `FRL3-FRIEND` host ack.
    FriendAck(Frl3FriendAck),
}

impl Frl3Message {
    /// `true` iff `payload` is a handshake message rather than a record:
    /// records open with a `u32be` length < [`MAX_E2EE_RECORD_BYTES`], so
    /// their first byte is at most 0x01 — never `b'F'`.
    pub fn is_handshake(payload: &[u8]) -> bool {
        payload.starts_with(b"FRL3")
    }

    /// Decode one handshake message; fail closed on bad length, magic or tag.
    pub fn decode(payload: &[u8]) -> Result<Self, E2eeError> {
        if payload.len() < 5 || payload[..4] != FRL3_MAGIC {
            return Err(E2eeError::Malformed);
        }
        let body = &payload[5..];
        let take32 = |i: usize| -> Result<[u8; 32], E2eeError> {
            body.get(i..i + 32)
                .map(|s| s.try_into().unwrap())
                .ok_or(E2eeError::Malformed)
        };
        match payload[4] {
            TAG_HELLO => {
                if body.len() != 64 {
                    return Err(E2eeError::Malformed);
                }
                Ok(Frl3Message::Hello(Frl3Hello {
                    guest_eph_pk: take32(0)?,
                    guest_mac: take32(32)?,
                }))
            }
            TAG_ACK => {
                if body.len() != 64 {
                    return Err(E2eeError::Malformed);
                }
                Ok(Frl3Message::Ack(Frl3Ack {
                    host_eph_pk: take32(0)?,
                    host_confirm: take32(32)?,
                }))
            }
            TAG_FRIEND_HELLO => {
                if body.len() != 112 {
                    return Err(E2eeError::Malformed);
                }
                Ok(Frl3Message::FriendHello(Frl3FriendHello {
                    guest_eph_pk: take32(0)?,
                    guest_static_pk: take32(32)?,
                    token: body[64..80]
                        .try_into()
                        .map_err(|_| E2eeError::Malformed)?,
                    proof: take32(80)?,
                }))
            }
            TAG_FRIEND_ACK => {
                if body.len() != 64 {
                    return Err(E2eeError::Malformed);
                }
                Ok(Frl3Message::FriendAck(Frl3FriendAck {
                    host_eph_pk: take32(0)?,
                    host_confirm: take32(32)?,
                }))
            }
            _ => Err(E2eeError::Malformed),
        }
    }

    /// Encode one handshake message: `b"FRL3" | tag | fields`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(117);
        out.extend_from_slice(&FRL3_MAGIC);
        match self {
            Frl3Message::Hello(h) => {
                out.push(TAG_HELLO);
                out.extend_from_slice(&h.guest_eph_pk);
                out.extend_from_slice(&h.guest_mac);
            }
            Frl3Message::Ack(a) => {
                out.push(TAG_ACK);
                out.extend_from_slice(&a.host_eph_pk);
                out.extend_from_slice(&a.host_confirm);
            }
            Frl3Message::FriendHello(h) => {
                out.push(TAG_FRIEND_HELLO);
                out.extend_from_slice(&h.guest_eph_pk);
                out.extend_from_slice(&h.guest_static_pk);
                out.extend_from_slice(&h.token);
                out.extend_from_slice(&h.proof);
            }
            Frl3Message::FriendAck(a) => {
                out.push(TAG_FRIEND_ACK);
                out.extend_from_slice(&a.host_eph_pk);
                out.extend_from_slice(&a.host_confirm);
            }
        }
        out
    }
}

// ===========================================================================
// `FRL3-PSK` handshake: ephemeral X25519 + invite-code PSK.
// ===========================================================================

fn hello_mac(code: &InviteCode, stream_id: u32, guest_name: &str, eph_pk: &[u8; 32]) -> [u8; 32] {
    let mut m = Vec::with_capacity(12 + 4 + guest_name.len() + 32);
    m.extend_from_slice(b"FRL3/ghello");
    m.extend_from_slice(&stream_id.to_be_bytes());
    m.extend_from_slice(guest_name.as_bytes());
    m.extend_from_slice(eph_pk);
    frl_auth::hmac_sha256(&code.as_bytes(), &m)
}

fn psk_keys(
    code: &InviteCode,
    stream_id: u32,
    guest_name: &str,
    geph: &[u8; 32],
    heph: &[u8; 32],
    shared: &[u8; 32],
) -> ([u8; 32], [u8; 32]) {
    let mut ikm = Vec::with_capacity(37);
    ikm.extend_from_slice(shared);
    ikm.extend_from_slice(&code.as_bytes());
    let prk = hkdf_extract(&code.as_bytes(), &ikm);
    let mut info = Vec::with_capacity(9 + 4 + guest_name.len() + 64);
    info.extend_from_slice(b"FRL3/keys");
    info.extend_from_slice(&stream_id.to_be_bytes());
    info.extend_from_slice(guest_name.as_bytes());
    info.extend_from_slice(geph);
    info.extend_from_slice(heph);
    let okm = hkdf_expand(&prk, &info, 64);
    let mut k_g2h = [0u8; 32];
    let mut k_h2g = [0u8; 32];
    k_g2h.copy_from_slice(&okm[..32]);
    k_h2g.copy_from_slice(&okm[32..]);
    (k_g2h, k_h2g)
}

fn host_confirm(k_h2g: &[u8; 32], stream_id: u32, geph: &[u8; 32], heph: &[u8; 32]) -> [u8; 32] {
    let mut m = Vec::with_capacity(15 + 4 + 64);
    m.extend_from_slice(b"FRL3/hconfirm");
    m.extend_from_slice(&stream_id.to_be_bytes());
    m.extend_from_slice(geph);
    m.extend_from_slice(heph);
    frl_auth::hmac_sha256(k_h2g, &m)
}

/// Direction tag for guest → host records.
const DIR_G2H: u8 = 0x01;
/// Direction tag for host → guest records.
const DIR_H2G: u8 = 0x02;

/// Sans-IO guest half of the `FRL3-PSK` handshake: [`start`](Self::start)
/// produces the [`Frl3Hello`] to send, [`finish`](Self::finish) consumes the
/// [`Frl3Ack`] and yields the ready [`Frl3Cipher`].
pub struct Frl3GuestHandshake {
    code: InviteCode,
    stream_id: u32,
    guest_name: String,
    eph_sk: [u8; 32],
    eph_pk: [u8; 32],
}

impl fmt::Debug for Frl3GuestHandshake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frl3GuestHandshake")
            .field("stream_id", &self.stream_id)
            .field("guest_name", &self.guest_name)
            .field("eph_sk", &"[REDACTED]")
            .finish()
    }
}

impl Frl3GuestHandshake {
    /// Begin the handshake with a fresh OS-CSPRNG ephemeral key.
    pub fn start(code: &InviteCode, stream_id: u32, guest_name: &str) -> (Self, Frl3Hello) {
        Self::start_with_secret(code, stream_id, guest_name, random_32_bytes())
    }

    /// Begin the handshake with a caller-supplied ephemeral secret
    /// (deterministic — for tests and reproducible sessions).
    pub fn start_with_secret(
        code: &InviteCode,
        stream_id: u32,
        guest_name: &str,
        eph_sk: [u8; 32],
    ) -> (Self, Frl3Hello) {
        let eph_pk = x25519_public(&eph_sk);
        let mac = hello_mac(code, stream_id, guest_name, &eph_pk);
        (
            Self {
                code: *code,
                stream_id,
                guest_name: guest_name.to_owned(),
                eph_sk,
                eph_pk,
            },
            Frl3Hello {
                guest_eph_pk: eph_pk,
                guest_mac: mac,
            },
        )
    }

    /// Complete the handshake: derive `shared`, both directional keys, and
    /// verify `host_confirm` in constant time. Any failure — a substituted
    /// ephemeral, a forged confirm, a low-order peer key — is an error and
    /// produces no cipher.
    pub fn finish(self, ack: &Frl3Ack) -> Result<Frl3Cipher, E2eeError> {
        let shared = dh(&self.eph_sk, &ack.host_eph_pk)?;
        let (k_g2h, k_h2g) = psk_keys(
            &self.code,
            self.stream_id,
            &self.guest_name,
            &self.eph_pk,
            &ack.host_eph_pk,
            &shared,
        );
        let expected = host_confirm(&k_h2g, self.stream_id, &self.eph_pk, &ack.host_eph_pk);
        if !frl_auth::ct_eq(&expected, &ack.host_confirm) {
            return Err(E2eeError::AuthenticationFailed);
        }
        let hello_bytes = Frl3Message::Hello(Frl3Hello {
            guest_eph_pk: self.eph_pk,
            guest_mac: hello_mac(&self.code, self.stream_id, &self.guest_name, &self.eph_pk),
        })
        .encode();
        let ack_bytes = Frl3Message::Ack(ack.clone()).encode();
        Ok(Frl3Cipher::guest(
            self.stream_id,
            k_g2h,
            k_h2g,
            transcript_hash(&hello_bytes, &ack_bytes),
        ))
    }
}

/// Host half of `FRL3-PSK` with a fresh OS-CSPRNG ephemeral key.
pub fn host_accept(
    code: &InviteCode,
    stream_id: u32,
    guest_name: &str,
    hello: &Frl3Hello,
) -> Result<(Frl3Cipher, Frl3Ack), E2eeError> {
    host_accept_with_secret(code, stream_id, guest_name, hello, random_32_bytes())
}

/// Host half of `FRL3-PSK` with a caller-supplied ephemeral secret. Verifies
/// `guest_mac` in constant time *before* touching the curve, rejects
/// low-order peer keys, then answers with `host_eph_pk` + `host_confirm`.
pub fn host_accept_with_secret(
    code: &InviteCode,
    stream_id: u32,
    guest_name: &str,
    hello: &Frl3Hello,
    eph_sk: [u8; 32],
) -> Result<(Frl3Cipher, Frl3Ack), E2eeError> {
    let expected = hello_mac(code, stream_id, guest_name, &hello.guest_eph_pk);
    if !frl_auth::ct_eq(&expected, &hello.guest_mac) {
        return Err(E2eeError::AuthenticationFailed);
    }
    let shared = dh(&eph_sk, &hello.guest_eph_pk)?;
    let eph_pk = x25519_public(&eph_sk);
    let (k_g2h, k_h2g) = psk_keys(
        code,
        stream_id,
        guest_name,
        &hello.guest_eph_pk,
        &eph_pk,
        &shared,
    );
    let confirm = host_confirm(&k_h2g, stream_id, &hello.guest_eph_pk, &eph_pk);
    let ack = Frl3Ack {
        host_eph_pk: eph_pk,
        host_confirm: confirm,
    };
    let hello_bytes = Frl3Message::Hello(hello.clone()).encode();
    let ack_bytes = Frl3Message::Ack(ack.clone()).encode();
    Ok((
        Frl3Cipher::host(
            stream_id,
            k_g2h,
            k_h2g,
            transcript_hash(&hello_bytes, &ack_bytes),
        ),
        ack,
    ))
}

// ===========================================================================
// `FRL3-FRIEND` handshake: mutual static + ephemeral X25519 (Noise-IK shape)
// with an allowlist, a 128-bit guest token and a SAS transcript code.
// ===========================================================================

/// A long-term `FRL3-FRIEND` identity: a 256-bit static X25519 keypair. The
/// public half renders as a copyable `megu-id:` string via
/// [`format_friend_id`].
#[derive(Clone)]
pub struct Frl3Identity {
    static_sk: [u8; 32],
    static_pk: [u8; 32],
}

impl fmt::Debug for Frl3Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frl3Identity")
            .field("friend_id", &self.friend_id())
            .field("static_sk", &"[REDACTED]")
            .finish()
    }
}

impl Frl3Identity {
    /// A fresh identity drawn from the OS CSPRNG.
    pub fn generate() -> Self {
        Self::from_secret(random_32_bytes())
    }

    /// Rebuild an identity from a stored 32-byte secret.
    pub fn from_secret(static_sk: [u8; 32]) -> Self {
        Self {
            static_sk,
            static_pk: x25519_public(&static_sk),
        }
    }

    /// The 256-bit static public key — the identity's public half.
    pub fn public_key(&self) -> [u8; 32] {
        self.static_pk
    }

    /// The copyable `megu-id:` friend string (with CRC32 checksum).
    pub fn friend_id(&self) -> String {
        format_friend_id(&self.static_pk)
    }
}

/// A 128-bit one-off guest token for `FRL3-FRIEND` allowlist entries that
/// require one — a fresh random secret, never a 40-bit code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frl3GuestToken(pub [u8; 16]);

impl Frl3GuestToken {
    /// The all-zero "no token presented" sentinel.
    pub const NONE: Self = Frl3GuestToken([0u8; 16]);

    /// A fresh token drawn from the OS CSPRNG.
    pub fn generate() -> Self {
        let r = random_32_bytes();
        let mut t = [0u8; 16];
        t.copy_from_slice(&r[..16]);
        Frl3GuestToken(t)
    }

    /// Wrap raw token bytes.
    pub fn from_bytes(b: [u8; 16]) -> Self {
        Frl3GuestToken(b)
    }

    /// The token as sixteen bytes.
    pub fn as_bytes(&self) -> [u8; 16] {
        self.0
    }
}

/// The host's trusted-peer set for `FRL3-FRIEND`: static public keys the
/// host has paired with, each optionally requiring a matching
/// [`Frl3GuestToken`] for one-off guests.
#[derive(Clone, Debug, Default)]
pub struct FriendAllowlist {
    /// `static_pk` → optional required guest token.
    entries: BTreeMap<[u8; 32], Option<[u8; 16]>>,
}

impl FriendAllowlist {
    /// An empty allowlist.
    pub fn new() -> Self {
        Self::default()
    }

    /// Trust `static_pk` with no additional token requirement.
    pub fn add(&mut self, static_pk: [u8; 32]) {
        self.entries.insert(static_pk, None);
    }

    /// Trust `static_pk` only when the peer also presents `token`.
    pub fn add_with_token(&mut self, static_pk: [u8; 32], token: Frl3GuestToken) {
        self.entries.insert(static_pk, Some(token.0));
    }

    /// Stop trusting `static_pk`. `true` iff it was listed.
    pub fn remove(&mut self, static_pk: &[u8; 32]) -> bool {
        self.entries.remove(static_pk).is_some()
    }

    /// Number of trusted peers.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` iff no peers are trusted.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `true` iff `static_pk` is listed at all (ignoring tokens).
    pub fn contains(&self, static_pk: &[u8; 32]) -> bool {
        self.entries.contains_key(static_pk)
    }

    /// Admission check: `static_pk` must be listed, and if its entry demands
    /// a token the presented one must match in constant time.
    pub fn allows(&self, static_pk: &[u8; 32], token: &Frl3GuestToken) -> bool {
        match self.entries.get(static_pk) {
            Some(None) => true,
            Some(Some(required)) => frl_auth::ct_eq(required, &token.0),
            None => false,
        }
    }
}

/// Crockford Base32 — the same alphabet the invite codes use.
const FRIEND_B32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

fn b32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() * 8).div_ceil(5));
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &b in data {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(FRIEND_B32[((acc >> bits) & 31) as usize]));
        }
    }
    if bits > 0 {
        out.push(char::from(FRIEND_B32[((acc << (5 - bits)) & 31) as usize]));
    }
    out
}

/// Fail-closed decode: tolerates case, hyphens and spaces plus the same
/// `o→0`/`i,l→1` homoglyphs as invite codes; anything else is `None`.
fn b32_decode(text: &str) -> Option<Vec<u8>> {
    let mut acc = 0u64;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(text.len() * 5 / 8);
    let mut leftover = 0u32;
    for ch in text.chars() {
        if ch == '-' || ch == ' ' {
            continue;
        }
        if !ch.is_ascii() {
            return None;
        }
        let digit = match ch.to_ascii_uppercase() {
            'O' => b'0',
            'I' | 'L' => b'1',
            other => other as u8,
        };
        let idx = FRIEND_B32.iter().position(|&c| c == digit)? as u64;
        acc = (acc << 5) | idx;
        bits += 5;
        while bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xFF) as u8);
            acc &= (1 << bits) - 1;
        }
        leftover = (acc & ((1 << bits) - 1)) as u32;
    }
    // Trailing padding bits must be present (<8) and zero — fail closed.
    if bits >= 8 || leftover != 0 {
        return None;
    }
    Some(out)
}

/// Render `static_pk` as `megu-id:<base32(pk || crc32(pk))>` — 58 Crockford
/// characters over 36 bytes.
pub fn format_friend_id(static_pk: &[u8; 32]) -> String {
    let mut data = [0u8; 36];
    data[..32].copy_from_slice(static_pk);
    data[32..].copy_from_slice(&megu_core::hash::crc32_ieee(static_pk).to_be_bytes());
    format!("megu-id:{}", b32_encode(&data))
}

/// Parse a `megu-id:` string back to the 32-byte static public key.
/// Tolerant on surface details (optional `megu-id:`/`megu` prefix, case,
/// hyphens, spaces, homoglyphs) and fail-closed on content: wrong decoded
/// length or a mismatched CRC32 is [`E2eeError::BadFriendId`].
pub fn parse_friend_id(text: &str) -> Result<[u8; 32], E2eeError> {
    let mut body = text.trim();
    let lower = body.to_ascii_lowercase();
    if let Some(stripped) = lower.strip_prefix("megu") {
        // "megu" ["-"|space]* ["id"] ["-"|space]* [":"] — the prefix's own
        // separator chars are interchangeable on purpose.
        let mut rest = stripped.trim_start_matches(['-', ' ']);
        if let Some(r) = rest.strip_prefix("id") {
            rest = r;
            rest = rest.trim_start_matches(['-', ' ']);
        }
        if let Some(r) = rest.strip_prefix(':') {
            rest = r;
        }
        body = &body[body.len() - rest.len()..];
    }
    let data = b32_decode(body.trim()).ok_or(E2eeError::BadFriendId)?;
    if data.len() != 36 {
        return Err(E2eeError::BadFriendId);
    }
    let pk: [u8; 32] = data[..32].try_into().unwrap();
    let got = u32::from_be_bytes(data[32..].try_into().unwrap());
    if got != megu_core::hash::crc32_ieee(&pk) {
        return Err(E2eeError::BadFriendId);
    }
    Ok(pk)
}

/// The guest's proof of static-key possession, keyed under `DH(s_g, s_h)`
/// (which only the guest and the real host can compute) and binding every
/// `FriendHello` field.
fn friend_proof(
    dh_ss: &[u8; 32],
    host_static_pk: &[u8; 32],
    stream_id: u32,
    guest_name: &str,
    guest_eph_pk: &[u8; 32],
    guest_static_pk: &[u8; 32],
    token: &[u8; 16],
) -> [u8; 32] {
    let proof_key = hkdf_extract(host_static_pk, dh_ss);
    let mut m = Vec::with_capacity(12 + 4 + guest_name.len() + 96);
    m.extend_from_slice(b"FRL3/fproof");
    m.extend_from_slice(&stream_id.to_be_bytes());
    m.extend_from_slice(guest_name.as_bytes());
    m.extend_from_slice(guest_eph_pk);
    m.extend_from_slice(guest_static_pk);
    m.extend_from_slice(token);
    frl_auth::hmac_sha256(&proof_key, &m)
}

/// The four-DH key schedule: `dh_ee | dh_es | dh_se | dh_ss` mixed through
/// HKDF with the full handshake context as `info`. Returns `(k_g2h, k_h2g)`.
fn friend_keys(
    stream_id: u32,
    guest_name: &str,
    guest_eph_pk: &[u8; 32],
    guest_static_pk: &[u8; 32],
    host_static_pk: &[u8; 32],
    host_eph_pk: &[u8; 32],
    dhs: &[[u8; 32]; 4],
) -> ([u8; 32], [u8; 32]) {
    let mut ikm = Vec::with_capacity(128);
    for d in dhs {
        ikm.extend_from_slice(d);
    }
    let prk = hkdf_extract(b"FRL3/ik", &ikm);
    let mut info = Vec::with_capacity(11 + 4 + guest_name.len() + 128);
    info.extend_from_slice(b"FRL3/fkeys");
    info.extend_from_slice(&stream_id.to_be_bytes());
    info.extend_from_slice(guest_name.as_bytes());
    info.extend_from_slice(guest_eph_pk);
    info.extend_from_slice(guest_static_pk);
    info.extend_from_slice(host_static_pk);
    info.extend_from_slice(host_eph_pk);
    let okm = hkdf_expand(&prk, &info, 64);
    let mut k_g2h = [0u8; 32];
    let mut k_h2g = [0u8; 32];
    k_g2h.copy_from_slice(&okm[..32]);
    k_h2g.copy_from_slice(&okm[32..]);
    (k_g2h, k_h2g)
}

fn friend_confirm(
    k_h2g: &[u8; 32],
    stream_id: u32,
    guest_eph_pk: &[u8; 32],
    guest_static_pk: &[u8; 32],
    host_eph_pk: &[u8; 32],
) -> [u8; 32] {
    let mut m = Vec::with_capacity(15 + 4 + 96);
    m.extend_from_slice(b"FRL3/fconfirm");
    m.extend_from_slice(&stream_id.to_be_bytes());
    m.extend_from_slice(guest_eph_pk);
    m.extend_from_slice(guest_static_pk);
    m.extend_from_slice(host_eph_pk);
    frl_auth::hmac_sha256(k_h2g, &m)
}

/// Sans-IO guest half of `FRL3-FRIEND`: the guest authenticates with its
/// long-term [`Frl3Identity`] to a host whose static public key it pinned
/// out-of-band (the `megu-id:` it paired with).
pub struct Frl3FriendGuestHandshake {
    stream_id: u32,
    guest_name: String,
    eph_sk: [u8; 32],
    eph_pk: [u8; 32],
    static_sk: [u8; 32],
    host_static_pk: [u8; 32],
    hello: Frl3FriendHello,
    dh_ss: [u8; 32],
    dh_es: [u8; 32],
}

impl fmt::Debug for Frl3FriendGuestHandshake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frl3FriendGuestHandshake")
            .field("stream_id", &self.stream_id)
            .field("guest_name", &self.guest_name)
            .field("eph_pk", &self.eph_pk)
            .field("secrets", &"[REDACTED]")
            .finish()
    }
}

impl Frl3FriendGuestHandshake {
    /// Begin the handshake with a fresh ephemeral key and the optional
    /// one-off [`Frl3GuestToken`] (pass [`Frl3GuestToken::NONE`] when unused).
    pub fn start(
        identity: &Frl3Identity,
        host_static_pk: &[u8; 32],
        stream_id: u32,
        guest_name: &str,
        token: Frl3GuestToken,
    ) -> Result<(Self, Frl3FriendHello), E2eeError> {
        Self::start_with_secret(identity, host_static_pk, stream_id, guest_name, token, random_32_bytes())
    }

    /// Begin with a caller-supplied ephemeral secret (deterministic tests).
    pub fn start_with_secret(
        identity: &Frl3Identity,
        host_static_pk: &[u8; 32],
        stream_id: u32,
        guest_name: &str,
        token: Frl3GuestToken,
        eph_sk: [u8; 32],
    ) -> Result<(Self, Frl3FriendHello), E2eeError> {
        let eph_pk = x25519_public(&eph_sk);
        let dh_ss = dh(&identity.static_sk, host_static_pk)?;
        let dh_es = dh(&eph_sk, host_static_pk)?;
        let proof = friend_proof(
            &dh_ss,
            host_static_pk,
            stream_id,
            guest_name,
            &eph_pk,
            &identity.static_pk,
            &token.0,
        );
        let hello = Frl3FriendHello {
            guest_eph_pk: eph_pk,
            guest_static_pk: identity.static_pk,
            token: token.0,
            proof,
        };
        Ok((
            Self {
                stream_id,
                guest_name: guest_name.to_owned(),
                eph_sk,
                eph_pk,
                static_sk: identity.static_sk,
                host_static_pk: *host_static_pk,
                hello: hello.clone(),
                dh_ss,
                dh_es,
            },
            hello,
        ))
    }

    /// Complete the handshake: compute `DH(e_g,e_h)` and `DH(s_g,e_h)`,
    /// derive the four-DH session keys, and verify `host_confirm` in
    /// constant time — which proves the peer holds the host's static secret.
    pub fn finish(self, ack: &Frl3FriendAck) -> Result<Frl3Cipher, E2eeError> {
        let dh_ee = dh(&self.eph_sk, &ack.host_eph_pk)?;
        let dh_se = dh(&self.static_sk, &ack.host_eph_pk)?;
        let (k_g2h, k_h2g) = friend_keys(
            self.stream_id,
            &self.guest_name,
            &self.eph_pk,
            &self.hello.guest_static_pk,
            &self.host_static_pk,
            &ack.host_eph_pk,
            &[dh_ee, self.dh_es, dh_se, self.dh_ss],
        );
        let expected = friend_confirm(
            &k_h2g,
            self.stream_id,
            &self.eph_pk,
            &self.hello.guest_static_pk,
            &ack.host_eph_pk,
        );
        if !frl_auth::ct_eq(&expected, &ack.host_confirm) {
            return Err(E2eeError::AuthenticationFailed);
        }
        let hello_bytes = Frl3Message::FriendHello(self.hello.clone()).encode();
        let ack_bytes = Frl3Message::FriendAck(ack.clone()).encode();
        Ok(Frl3Cipher::guest(
            self.stream_id,
            k_g2h,
            k_h2g,
            transcript_hash(&hello_bytes, &ack_bytes),
        ))
    }
}

/// Host half of `FRL3-FRIEND` with a fresh OS-CSPRNG ephemeral key.
pub fn friend_host_accept(
    host_identity: &Frl3Identity,
    allowlist: &FriendAllowlist,
    stream_id: u32,
    guest_name: &str,
    hello: &Frl3FriendHello,
) -> Result<(Frl3Cipher, Frl3FriendAck), E2eeError> {
    friend_host_accept_with_secret(
        host_identity,
        allowlist,
        stream_id,
        guest_name,
        hello,
        random_32_bytes(),
    )
}

/// Host half of `FRL3-FRIEND` with a caller-supplied ephemeral secret.
/// Fail-closed order: unknown static key or mismatched token →
/// [`E2eeError::UnknownPeer`] (before any DH work); a bad proof →
/// [`E2eeError::AuthenticationFailed`]; a degenerate key →
/// [`E2eeError::ContributorySecret`].
pub fn friend_host_accept_with_secret(
    host_identity: &Frl3Identity,
    allowlist: &FriendAllowlist,
    stream_id: u32,
    guest_name: &str,
    hello: &Frl3FriendHello,
    eph_sk: [u8; 32],
) -> Result<(Frl3Cipher, Frl3FriendAck), E2eeError> {
    if !allowlist.allows(&hello.guest_static_pk, &Frl3GuestToken(hello.token)) {
        return Err(E2eeError::UnknownPeer);
    }
    let dh_ss = dh(&host_identity.static_sk, &hello.guest_static_pk)?;
    let expected = friend_proof(
        &dh_ss,
        &host_identity.static_pk,
        stream_id,
        guest_name,
        &hello.guest_eph_pk,
        &hello.guest_static_pk,
        &hello.token,
    );
    if !frl_auth::ct_eq(&expected, &hello.proof) {
        return Err(E2eeError::AuthenticationFailed);
    }
    let eph_pk = x25519_public(&eph_sk);
    let dh_ee = dh(&eph_sk, &hello.guest_eph_pk)?;
    let dh_es = dh(&host_identity.static_sk, &hello.guest_eph_pk)?;
    let dh_se = dh(&eph_sk, &hello.guest_static_pk)?;
    let (k_g2h, k_h2g) = friend_keys(
        stream_id,
        guest_name,
        &hello.guest_eph_pk,
        &hello.guest_static_pk,
        &host_identity.static_pk,
        &eph_pk,
        &[dh_ee, dh_es, dh_se, dh_ss],
    );
    let confirm = friend_confirm(
        &k_h2g,
        stream_id,
        &hello.guest_eph_pk,
        &hello.guest_static_pk,
        &eph_pk,
    );
    let ack = Frl3FriendAck {
        host_eph_pk: eph_pk,
        host_confirm: confirm,
    };
    let hello_bytes = Frl3Message::FriendHello(hello.clone()).encode();
    let ack_bytes = Frl3Message::FriendAck(ack.clone()).encode();
    Ok((
        Frl3Cipher::host(
            stream_id,
            k_g2h,
            k_h2g,
            transcript_hash(&hello_bytes, &ack_bytes),
        ),
        ack,
    ))
}

fn transcript_hash(hello_bytes: &[u8], ack_bytes: &[u8]) -> [u8; 32] {
    let mut t = Vec::with_capacity(hello_bytes.len() + ack_bytes.len());
    t.extend_from_slice(hello_bytes);
    t.extend_from_slice(ack_bytes);
    frl_auth::sha256(&t)
}

/// Six-digit Short Authentication String over a handshake transcript hash —
/// the optional visual fingerprint for pairing a new friend.
pub fn sas_code(transcript_hash: &[u8; 32]) -> u32 {
    u32::from_be_bytes(transcript_hash[..4].try_into().unwrap()) % 1_000_000
}

// ===========================================================================
// `Frl3Cipher`: the directional record layer.
// ===========================================================================

fn record_nonce(dir: u8, seq: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[0] = dir;
    n[4..].copy_from_slice(&seq.to_be_bytes());
    n
}

fn record_aad(dir: u8, stream_id: u32, seq: u64) -> [u8; 17] {
    let mut a = [0u8; 17];
    a[..4].copy_from_slice(b"FRL3");
    a[4] = dir;
    a[5..9].copy_from_slice(&stream_id.to_be_bytes());
    a[9..].copy_from_slice(&seq.to_be_bytes());
    a
}

/// An established `FRL3` session: directional ChaCha20-Poly1305 keys with
/// strict-monotonic `u64` sequence numbers. `seal` produces
/// `u32be(ct_len) | ciphertext | tag(16)` records; `open` consumes exactly
/// that shape. Both fail closed on overflow — a nonce never wraps.
pub struct Frl3Cipher {
    stream_id: u32,
    send_key: [u8; 32],
    send_dir: u8,
    send_seq: u64,
    recv_key: [u8; 32],
    recv_dir: u8,
    recv_seq: u64,
    transcript: [u8; 32],
}

impl fmt::Debug for Frl3Cipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frl3Cipher")
            .field("stream_id", &self.stream_id)
            .field("send_seq", &self.send_seq)
            .field("recv_seq", &self.recv_seq)
            .field("keys", &"[REDACTED]")
            .finish()
    }
}

impl Frl3Cipher {
    /// The guest view: sends under `k_g2h`/dir 0x01, receives under
    /// `k_h2g`/dir 0x02.
    fn guest(stream_id: u32, k_g2h: [u8; 32], k_h2g: [u8; 32], transcript: [u8; 32]) -> Self {
        Self {
            stream_id,
            send_key: k_g2h,
            send_dir: DIR_G2H,
            send_seq: 0,
            recv_key: k_h2g,
            recv_dir: DIR_H2G,
            recv_seq: 0,
            transcript,
        }
    }

    /// The host view — the mirror image of [`guest`](Self::guest).
    fn host(stream_id: u32, k_g2h: [u8; 32], k_h2g: [u8; 32], transcript: [u8; 32]) -> Self {
        Self {
            stream_id,
            send_key: k_h2g,
            send_dir: DIR_H2G,
            send_seq: 0,
            recv_key: k_g2h,
            recv_dir: DIR_G2H,
            recv_seq: 0,
            transcript,
        }
    }

    /// The `FRL2` stream this cipher is bound to (also bound in the AAD).
    pub fn stream_id(&self) -> u32 {
        self.stream_id
    }

    /// The six-digit SAS for this session's transcript — compare visually
    /// with the peer when pairing.
    pub fn sas_code(&self) -> u32 {
        sas_code(&self.transcript)
    }

    /// Seal `plaintext` into a `u32be(ct_len) | ciphertext | tag(16)`
    /// record, bound to this direction, this stream and the next sequence
    /// number. Fails closed on the size cap and on counter exhaustion.
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, E2eeError> {
        if plaintext.len() > MAX_E2EE_RECORD_BYTES {
            return Err(E2eeError::RecordTooLarge);
        }
        let seq = self.send_seq;
        self.send_seq = seq.checked_add(1).ok_or(E2eeError::CounterExhausted)?;
        let nonce = record_nonce(self.send_dir, seq);
        let aad = record_aad(self.send_dir, self.stream_id, seq);
        let sealed = aead_seal(&self.send_key, &nonce, &aad, plaintext);
        let mut out = Vec::with_capacity(4 + sealed.len());
        out.extend_from_slice(&(plaintext.len() as u32).to_be_bytes());
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    /// Open one record produced by [`seal`](Self::seal). The declared
    /// `ct_len` is capped *before* slicing, the AAD embeds the expected
    /// sequence number — so a replayed, reordered, bit-flipped or
    /// cross-direction record can never verify.
    pub fn open(&mut self, record: &[u8]) -> Result<Vec<u8>, E2eeError> {
        if record.len() < 4 + 16 {
            return Err(E2eeError::Malformed);
        }
        let ct_len = u32::from_be_bytes(record[..4].try_into().unwrap()) as usize;
        if ct_len > MAX_E2EE_RECORD_BYTES {
            return Err(E2eeError::RecordTooLarge);
        }
        if record.len() != 4 + ct_len + 16 {
            return Err(E2eeError::Malformed);
        }
        let seq = self.recv_seq;
        let nonce = record_nonce(self.recv_dir, seq);
        let aad = record_aad(self.recv_dir, self.stream_id, seq);
        let pt = aead_open(&self.recv_key, &nonce, &aad, &record[4..])?;
        self.recv_seq = seq.checked_add(1).ok_or(E2eeError::CounterExhausted)?;
        Ok(pt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_roundtrip_and_bounds() {
        let (guest_hs, hello) =
            Frl3GuestHandshake::start_with_secret(&InviteCode::from_bytes([7; 5]), 42, "g", [9; 32]);
        let (mut host, ack) = host_accept_with_secret(
            &InviteCode::from_bytes([7; 5]),
            42,
            "g",
            &hello,
            [3; 32],
        )
        .unwrap();
        let mut guest = guest_hs.finish(&ack).unwrap();
        assert_eq!(guest.sas_code(), host.sas_code());
        let r = guest.seal(b"hello world").unwrap();
        assert_eq!(host.open(&r).unwrap(), b"hello world");
        let r2 = host.seal(b"reply").unwrap();
        assert_eq!(guest.open(&r2).unwrap(), b"reply");
        // Replays and oversize records fail closed.
        assert!(host.open(&r).is_err());
        assert!(host.open(&[0u8; 4]).is_err());
    }

    #[test]
    fn crc32_known_value() {
        // IEEE CRC32 of "123456789" is the textbook 0xCBF43926.
        assert_eq!(megu_core::hash::crc32_ieee(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn friend_id_roundtrip_and_rejects() {
        let id = Frl3Identity::from_secret([5; 32]);
        let text = id.friend_id();
        assert!(text.starts_with("megu-id:"));
        assert_eq!(parse_friend_id(&text).unwrap(), id.public_key());
        assert_eq!(parse_friend_id(&text.to_uppercase()).unwrap(), id.public_key());
        assert_eq!(
            parse_friend_id(&text.replace('-', " ")).unwrap(),
            id.public_key()
        );
        // Corrupted tail char flips the checksum.
        let mut bad = text.clone();
        bad.replace_range(bad.len() - 1.., "0");
        if bad == text {
            bad.replace_range(bad.len() - 1.., "1");
        }
        assert_eq!(parse_friend_id(&bad), Err(E2eeError::BadFriendId));
        assert_eq!(parse_friend_id("megu-id:!!!"), Err(E2eeError::BadFriendId));
    }
}
