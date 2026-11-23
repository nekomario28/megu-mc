// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `FRL2` control-channel authentication: pure-`std` SHA-256 and HMAC
//! (FIPS 180-4 / RFC 2104) plus the domain-separated tags the friend-relay
//! protocol MACs its privileged frames with.
//!
//! ## Why these helpers exist
//!
//! `FRL1` carried the host's revoke credential on the wire verbatim — once at
//! `HostRegister` and again on every `HostRevoke` — and sent the guest's
//! invite code in clear on every `GuestJoin`. Any passive observer of either
//! frame owned the credential. `FRL2` replaces both with nonce-bound
//! HMAC-SHA256 proofs:
//!
//! * The relay issues one random `nonce` per TCP connection in `RelayHello`.
//! * `HostRevoke` carries [`revoke_mac`] instead of the raw key, so a sniffed
//!   frame proves nothing on another connection.
//! * `GuestJoin` carries [`join_key_id`] + [`join_tag`] — the invite code
//!   itself never crosses the wire, and a captured pair is useless under a
//!   different nonce.
//!
//! What this does **not** fix: the `session_key` still travels once inside
//! `HostRegister`, so an observer of that one frame can forge revokes for
//! that session; and an *active* MITM can still splice into unauthenticated
//! `StreamData`. Closing those needs real transport security (TLS/QUIC),
//! which is deliberately out of scope for this zero-dependency crate.

/// FIPS 180-4 SHA-256 over `data`.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
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
                .wrapping_add(K[i])
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
        for (i, v) in [a, b, c, d, e, f, g, hh].iter().enumerate() {
            h[i] = h[i].wrapping_add(*v);
        }
    }
    let mut out = [0u8; 32];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

/// RFC 2104 HMAC-SHA256.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&sha256(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Vec::with_capacity(64 + message.len());
    inner.extend(block.iter().map(|b| b ^ 0x36));
    inner.extend_from_slice(message);
    let inner_hash = sha256(&inner);
    let mut outer = Vec::with_capacity(96);
    outer.extend(block.iter().map(|b| b ^ 0x5c));
    outer.extend_from_slice(&inner_hash);
    sha256(&outer)
}

/// Constant-time byte equality: mismatched length fails fast, matching
/// length never branches on content.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// `HostRevoke` proof: the host proves it holds `session_key` without
/// sending it. Bound to this connection's `nonce` so a captured MAC cannot
/// be replayed on a later connection.
pub fn revoke_mac(session_key: &[u8; 16], invite_code: &[u8; 5], nonce: &[u8; 16]) -> [u8; 32] {
    let mut msg = Vec::with_capacity(4 + 4 + 5 + 16);
    msg.extend_from_slice(b"FRL2");
    msg.extend_from_slice(b"/rev");
    msg.extend_from_slice(invite_code);
    msg.extend_from_slice(nonce);
    hmac_sha256(session_key, &msg)
}

/// `GuestJoin` host selector: an 8-byte pseudonym for the invite code under
/// this connection's `nonce`. The code itself never appears on the wire;
/// the relay recomputes this for each live invite to find the intended
/// host.
pub fn join_key_id(invite_code: &[u8; 5], nonce: &[u8; 16]) -> [u8; 8] {
    let mut msg = Vec::with_capacity(4 + 3 + 16);
    msg.extend_from_slice(b"FRL2");
    msg.extend_from_slice(b"/id");
    msg.extend_from_slice(nonce);
    let mut out = [0u8; 8];
    out.copy_from_slice(&hmac_sha256(invite_code, &msg)[..8]);
    out
}

/// `GuestJoin` proof: binds this `key_id` and `guest_name` to the invite
/// code and the connection nonce. `key_id` already binds the nonce, so the
/// tag input does not repeat it.
pub fn join_tag(invite_code: &[u8; 5], key_id: &[u8; 8], guest_name: &str) -> [u8; 32] {
    let mut msg = Vec::with_capacity(4 + 5 + 8 + guest_name.len());
    msg.extend_from_slice(b"FRL2");
    msg.extend_from_slice(b"/join");
    msg.extend_from_slice(key_id);
    msg.extend_from_slice(guest_name.as_bytes());
    hmac_sha256(invite_code, &msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_fips_vectors() {
        assert_eq!(
            sha256(b"abc"),
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d,
                0xae, 0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10,
                0xff, 0x61, 0xf2, 0x00, 0x15, 0xad
            ]
        );
        assert_eq!(
            sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            [
                0x24, 0x8d, 0x6a, 0x61, 0xd2, 0x06, 0x38, 0xb8, 0xe5, 0xc0, 0x26, 0x93, 0x0c,
                0x3e, 0x60, 0x39, 0xa3, 0x3c, 0xe4, 0x59, 0x64, 0xff, 0x21, 0x67, 0xf6, 0xec,
                0xed, 0xd4, 0x19, 0xdb, 0x06, 0xc1
            ]
        );
    }

    #[test]
    fn hmac_sha256_rfc4231_vectors() {
        // RFC 4231 test case 1: 20-byte key 0x0b, "Hi There".
        assert_eq!(
            hmac_sha256(&[0x0b; 20], b"Hi There"),
            [
                0xb0, 0x34, 0x4c, 0x61, 0xd8, 0xdb, 0x38, 0x53, 0x5c, 0xa8, 0xaf, 0xce, 0xaf,
                0x0b, 0xf1, 0x2b, 0x88, 0x1d, 0xc2, 0x00, 0xc9, 0x83, 0x3d, 0xa7, 0x26, 0xe9,
                0x37, 0x6c, 0x2e, 0x32, 0xcf, 0xf7
            ]
        );
        // RFC 4231 test case 2: key "Jefe", "what do ya want for nothing?".
        assert_eq!(
            hmac_sha256(b"Jefe", b"what do ya want for nothing?"),
            [
                0x5b, 0xdc, 0xc1, 0x46, 0xbf, 0x60, 0x75, 0x4e, 0x6a, 0x04, 0x24, 0x26, 0x08,
                0x95, 0x75, 0xc7, 0x5a, 0x00, 0x3f, 0x08, 0x9d, 0x27, 0x39, 0x83, 0x9d, 0xec,
                0x58, 0xb9, 0x64, 0xec, 0x38, 0x43
            ]
        );
        // Key longer than the 64-byte block exercises the key-hashing path.
        let long_key = [0xAA; 131];
        assert_eq!(
            hmac_sha256(&long_key, b"Test Using Larger Than Block-Size Key - Hash Key First"),
            [
                0x60, 0xe4, 0x31, 0x59, 0x1e, 0xe0, 0xb6, 0x7f, 0x0d, 0x8a, 0x26, 0xaa, 0xcb,
                0xf5, 0xb7, 0x7f, 0x8e, 0x0b, 0xc6, 0x21, 0x37, 0x28, 0xc5, 0x14, 0x05, 0x46,
                0x04, 0x0f, 0x0e, 0xe3, 0x7f, 0x54
            ]
        );
    }

    #[test]
    fn ct_eq_behaviour() {
        assert!(ct_eq(&[1, 2, 3], &[1, 2, 3]));
        assert!(!ct_eq(&[1, 2, 3], &[1, 2, 4]));
        assert!(!ct_eq(&[1, 2], &[1, 2, 3]));
        assert!(!ct_eq(&[], &[0]));
        assert!(ct_eq(&[] as &[u8], &[]));
    }

    #[test]
    fn revoke_mac_binds_key_code_and_nonce() {
        let key = [0x11; 16];
        let code = [0x22; 5];
        let nonce = [0x33; 16];
        let mac = revoke_mac(&key, &code, &nonce);
        // Same inputs are stable; each changed input changes the tag.
        assert_eq!(mac, revoke_mac(&key, &code, &nonce));
        assert_ne!(mac, revoke_mac(&[0x12; 16], &code, &nonce));
        assert_ne!(mac, revoke_mac(&key, &[0x23; 5], &nonce));
        assert_ne!(mac, revoke_mac(&key, &code, &[0x34; 16]));
    }

    #[test]
    fn join_proofs_bind_nonce() {
        let code = [0x42; 5];
        let nonce = [0x01; 16];
        let key_id = join_key_id(&code, &nonce);
        assert_eq!(key_id, join_key_id(&code, &nonce));
        // A different connection nonce rekeys the pseudonym entirely.
        assert_ne!(key_id, join_key_id(&code, &[0x02; 16]));
        let tag = join_tag(&code, &key_id, "alice");
        assert_eq!(tag, join_tag(&code, &key_id, "alice"));
        // Code, key_id and name all participate in the tag.
        assert_ne!(tag, join_tag(&[0x43; 5], &key_id, "alice"));
        assert_ne!(tag, join_tag(&code, &[0x99; 8], "alice"));
        assert_ne!(tag, join_tag(&code, &key_id, "bob"));
    }
}
