// SPDX-License-Identifier: LGPL-3.0-or-later
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

/// FIPS 180-4 SHA-256 over `data` — the canonical `megu_core::crypto`
/// implementation shared by the whole workspace.
pub use megu_core::crypto::sha256;

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
