// SPDX-License-Identifier: LGPL-3.0-or-later
//! Adversarial proof for the `FRL3` E2EE layer in [`megu_proto::frl_e2ee`]:
//! bit-exact RFC 7748 (X25519) and RFC 8439 (ChaCha20-Poly1305) test
//! vectors, the RFC 5869 HKDF vector, then a full Host ↔ `TcpFriendRelay` ↔
//! Guest loopback where the relay provably forwards only ciphertext — and a
//! battery of evil-relay tests: one-bit flips, replays, reorders, MITM key
//! substitution without the invite code, low-order key injection, allowlist
//! and guest-token rejections.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use megu_proto::frl_auth;
use megu_proto::frl_e2ee::{
    self, aead_open, aead_seal, format_friend_id, friend_host_accept_with_secret, hkdf_expand,
    hkdf_extract, parse_friend_id, x25519, x25519_public, E2eeError, FriendAllowlist,
    Frl3Ack, Frl3Cipher, Frl3FriendAck, Frl3FriendGuestHandshake, Frl3GuestHandshake,
    Frl3GuestToken, Frl3Hello, Frl3Identity, Frl3Message,
};
use megu_proto::friend_relay::{
    decode_packet, encode_frame, FriendPacket, Frl3Stream, InviteCode, TcpFriendRelay,
    MAX_FRL2_FRAME_BYTES,
};

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn as32(v: &[u8]) -> &[u8; 32] {
    v.try_into().unwrap()
}

// ===========================================================================
// Official RFC test vectors
// ===========================================================================

#[test]
fn x25519_rfc7748_section_6_1() {
    // RFC 7748 §6.1: two scalar multiplications.
    let k1 = hex("a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4");
    let u1 = hex("e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c");
    assert_eq!(
        x25519(as32(&k1), as32(&u1)).as_slice(),
        hex("c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552")
    );

    let k2 = hex("4b66e9d4d1b4673c5ad22691957d6af5c11b6421e0ea01d42ca4169e7918ba0d");
    let u2 = hex("e5210f12786811d3f4b7959d0538ae2c31dbe7106fc03c3efc4cd549c715a493");
    assert_eq!(
        x25519(as32(&k2), as32(&u2)).as_slice(),
        hex("95cbde9476e8907d7aade45cb4b873f88b595a68799fa152e6f8f7647aac7957")
    );

    // RFC 7748 §6.1 iterated: k = u = 0x09…, after 1 and 1000 iterations.
    let mut k = [0u8; 32];
    let mut u = [0u8; 32];
    k[0] = 9;
    u[0] = 9;
    for i in 0..1000 {
        let r = x25519(&k, &u);
        u = k;
        k = r;
        if i == 0 {
            assert_eq!(
                k.as_slice(),
                hex("422c8e7a6227d7bca1350b3e2bb7279f7897b87bb6854b783c60e80311ae3079")
            );
        }
    }
    assert_eq!(
        k.as_slice(),
        hex("684cf59ba83309552800ef566f2f4d3c1c3887c49360e3875f2eb94d99532c51")
    );
}

#[test]
fn chacha20_poly1305_rfc8439_section_2_8_2() {
    // RFC 8439 §2.8.2 — the "sunscreen" AEAD test vector.
    let key: [u8; 32] = hex("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f")
        .as_slice()
        .try_into()
        .unwrap();
    let nonce: [u8; 12] = hex("070000004041424344454647")
        .as_slice()
        .try_into()
        .unwrap();
    let aad = hex("50515253c0c1c2c3c4c5c6c7");
    let plaintext =
        b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
    let expected_ct = hex("d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d63dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b3692ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc3ff4def08e4b7a9de576d26586cec64b6116");
    let expected_tag = hex("1ae10b594f09e26a7e902ecbd0600691");

    let sealed = aead_seal(&key, &nonce, &aad, plaintext);
    assert_eq!(sealed.len(), expected_ct.len() + 16);
    assert_eq!(&sealed[..expected_ct.len()], expected_ct.as_slice());
    assert_eq!(&sealed[expected_ct.len()..], expected_tag.as_slice());
    assert_eq!(aead_open(&key, &nonce, &aad, &sealed).unwrap(), plaintext);
    // One flipped tag bit → fail closed.
    let mut bad = sealed.clone();
    *bad.last_mut().unwrap() ^= 1;
    assert_eq!(
        aead_open(&key, &nonce, &aad, &bad),
        Err(E2eeError::AuthenticationFailed)
    );
}

#[test]
fn hkdf_sha256_rfc5869_case_1() {
    // RFC 5869 test case 1 (SHA-256, L=42).
    let ikm = [0x0bu8; 22];
    let salt = hex("000102030405060708090a0b0c");
    let info = hex("f0f1f2f3f4f5f6f7f8f9");
    let prk = hkdf_extract(&salt, &ikm);
    assert_eq!(
        prk.as_slice(),
        hex("077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5")
    );
    let okm = hkdf_expand(&prk, &info, 42);
    assert_eq!(
        okm,
        hex("3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865")
    );
}

// ===========================================================================
// Sans-IO handshake + record-layer adversarial tests
// ===========================================================================

const CODE: [u8; 5] = [0xAB; 5];

/// An established PSK cipher pair on stream 7 (deterministic secrets).
fn psk_pair() -> (Frl3Cipher, Frl3Cipher) {
    let code = InviteCode::from_bytes(CODE);
    let (g, hello) = Frl3GuestHandshake::start_with_secret(&code, 7, "Alice", [0x11; 32]);
    let (h, ack) =
        frl_e2ee::host_accept_with_secret(&code, 7, "Alice", &hello, [0x22; 32]).unwrap();
    let g = g.finish(&ack).unwrap();
    (g, h)
}

#[test]
fn psk_cipher_rejects_bitflip_replay_and_reorder() {
    let (mut g, mut h) = psk_pair();
    let r1 = g.seal(b"msg-one").unwrap();
    let r2 = g.seal(b"msg-two").unwrap();

    // One flipped bit anywhere in the record → hard failure, and the
    // rejection never advances the receive counter. Tampering with the
    // 4-byte length prefix fails closed with RecordTooLarge/Truncated;
    // tampering with ciphertext or tag bytes fails with
    // AuthenticationFailed.
    let mut bad_len = r1.clone();
    bad_len[0] ^= 0x01;
    assert!(h.open(&bad_len).is_err());
    for pos in [4, 10, r1.len() - 1] {
        let mut bad = r1.clone();
        bad[pos] ^= 0x01;
        assert_eq!(
            h.open(&bad),
            Err(E2eeError::AuthenticationFailed),
            "flip at byte {pos}"
        );
    }
    // Reorder: r2 (seq 1) before r1 (seq 0) — the AAD embeds the expected
    // sequence so it cannot verify out of order.
    assert_eq!(h.open(&r2), Err(E2eeError::AuthenticationFailed));
    // Now in order: r1 lands, and a replay of r1 is rejected.
    assert_eq!(h.open(&r1).unwrap(), b"msg-one");
    assert_eq!(h.open(&r1), Err(E2eeError::AuthenticationFailed));
    assert_eq!(h.open(&r2).unwrap(), b"msg-two");

    // Records are bound to direction: a host→guest record never opens on
    // the host's receive (guest→host) side.
    let h2g = h.seal(b"down").unwrap();
    assert_eq!(h.open(&h2g), Err(E2eeError::AuthenticationFailed));
    assert_eq!(g.open(&h2g).unwrap(), b"down");
}

#[test]
fn psk_mitm_substitution_fails_closed() {
    let code = InviteCode::from_bytes(CODE);
    let (g, hello) = Frl3GuestHandshake::start_with_secret(&code, 7, "Alice", [0x11; 32]);

    // (a) Evil relay swaps the ephemeral public key: it cannot fix the
    // invite-code-keyed MAC → the host rejects the hello outright.
    let mut evil_hello = hello.clone();
    evil_hello.guest_eph_pk = x25519_public(&[0xEE; 32]);
    assert!(matches!(
        frl_e2ee::host_accept(&code, 7, "Alice", &evil_hello),
        Err(E2eeError::AuthenticationFailed)
    ));

    // (b) MITM answers the guest with its own ephemeral + a made-up
    // confirm: without the invite code it cannot derive k_h2g → the
    // guest's constant-time confirm check fails closed.
    let evil_ack = Frl3Ack {
        host_eph_pk: x25519_public(&[0xEE; 32]),
        host_confirm: [0x55; 32],
    };
    assert!(matches!(
        g.finish(&evil_ack),
        Err(E2eeError::AuthenticationFailed)
    ));

    // (c) A different invite code on either side → the MAC never verifies;
    // the stream id and guest name are bound into the MAC too.
    let other = InviteCode::from_bytes([0xCD; 5]);
    for (c, s, n) in [(other, 7, "Alice"), (code, 8, "Alice"), (code, 7, "Mallory")] {
        assert!(matches!(
            frl_e2ee::host_accept(&c, s, n, &hello),
            Err(E2eeError::AuthenticationFailed)
        ));
    }
}

#[test]
fn psk_low_order_public_key_rejected() {
    let code = InviteCode::from_bytes(CODE);
    // A hello carrying the all-zero (low-order) ephemeral public key — with
    // a *valid* invite-code MAC for that key — must still fail closed on
    // the contributory-secret check.
    let zero = [0u8; 32];
    let mut m = Vec::new();
    m.extend_from_slice(b"FRL3/ghello");
    m.extend_from_slice(&7u32.to_be_bytes());
    m.extend_from_slice(b"Alice");
    m.extend_from_slice(&zero);
    let mac = frl_auth::hmac_sha256(&CODE, &m);
    let hello = Frl3Hello {
        guest_eph_pk: zero,
        guest_mac: mac,
    };
    assert!(matches!(
        frl_e2ee::host_accept(&code, 7, "Alice", &hello),
        Err(E2eeError::ContributorySecret)
    ));
}

#[test]
fn friend_id_format_parse_and_checksum() {
    let id = Frl3Identity::from_secret([0x42; 32]);
    let text = id.friend_id();
    assert!(text.starts_with("megu-id:"));
    assert_eq!(parse_friend_id(&text).unwrap(), id.public_key());
    assert_eq!(format_friend_id(&id.public_key()), text);
    // Tolerant on case and the optional prefix; fail closed on content.
    assert_eq!(
        parse_friend_id(&text.replace("megu-id:", "MEGU-ID-")).unwrap(),
        id.public_key()
    );
    let mut bad = text.clone();
    let last = bad.pop().unwrap();
    bad.push(if last == '0' { '1' } else { '0' });
    assert_eq!(parse_friend_id(&bad), Err(E2eeError::BadFriendId));
    assert_eq!(parse_friend_id("megu-id:0"), Err(E2eeError::BadFriendId));
}

fn friend_identities() -> (Frl3Identity, Frl3Identity, FriendAllowlist) {
    let host = Frl3Identity::from_secret([0x50; 32]);
    let guest = Frl3Identity::from_secret([0x60; 32]);
    let mut list = FriendAllowlist::new();
    list.add(guest.public_key());
    (host, guest, list)
}

#[test]
fn friend_handshake_allowlist_and_mitm() {
    let (host, guest, list) = friend_identities();
    let host_pk = host.public_key();

    // Happy path: both sides derive one cipher pair, SAS codes match.
    let (g, hello) =
        Frl3FriendGuestHandshake::start(&guest, &host_pk, 9, "Alice", Frl3GuestToken::NONE).unwrap();
    let (mut hc, ack) =
        friend_host_accept_with_secret(&host, &list, 9, "Alice", &hello, [0x77; 32]).unwrap();
    let mut gc = g.finish(&ack).unwrap();
    assert_eq!(gc.sas_code(), hc.sas_code());
    let r = gc.seal(b"friend bytes").unwrap();
    assert_eq!(hc.open(&r).unwrap(), b"friend bytes");

    // An unknown static pk — even one that knows the host's public key —
    // is rejected fail-closed before any DH work.
    let stranger = Frl3Identity::from_secret([0x99; 32]);
    let (_sg, shelo) =
        Frl3FriendGuestHandshake::start(&stranger, &host_pk, 9, "Mallory", Frl3GuestToken::NONE)
            .unwrap();
    assert!(matches!(
        friend_host_accept_with_secret(&host, &list, 9, "Mallory", &shelo, [0x77; 32]),
        Err(E2eeError::UnknownPeer)
    ));

    // A hello advertising `guest`'s listed pk but proven by `stranger`'s
    // secret → the proof (keyed by DH(s_g, s_h)) cannot verify.
    let mut forged = shelo.clone();
    forged.guest_static_pk = guest.public_key();
    assert!(matches!(
        friend_host_accept_with_secret(&host, &list, 9, "Mallory", &forged, [0x77; 32]),
        Err(E2eeError::AuthenticationFailed)
    ));

    // A malicious relay cannot MITM: it knows host_static_pk (public) but
    // not the secrets — a forged FriendAck fails the confirm check.
    let (g2, _h2) =
        Frl3FriendGuestHandshake::start(&guest, &host_pk, 9, "Alice", Frl3GuestToken::NONE).unwrap();
    let evil_ack = Frl3FriendAck {
        host_eph_pk: x25519_public(&[0xAB; 32]),
        host_confirm: [0xCD; 32],
    };
    assert!(matches!(
        g2.finish(&evil_ack),
        Err(E2eeError::AuthenticationFailed)
    ));
}

#[test]
fn friend_allowlist_token_gate() {
    let host = Frl3Identity::from_secret([0x50; 32]);
    let guest = Frl3Identity::from_secret([0x61; 32]);
    let token = Frl3GuestToken::generate();
    let mut list = FriendAllowlist::new();
    list.add_with_token(guest.public_key(), token);
    let host_pk = host.public_key();

    // Right token → admitted.
    let (_g, hello) = Frl3FriendGuestHandshake::start(&guest, &host_pk, 1, "g", token).unwrap();
    assert!(friend_host_accept_with_secret(&host, &list, 1, "g", &hello, [0x33; 32]).is_ok());

    // No token / wrong token → UnknownPeer, fail closed.
    let (_g, hello_none) =
        Frl3FriendGuestHandshake::start(&guest, &host_pk, 1, "g", Frl3GuestToken::NONE).unwrap();
    assert!(matches!(
        friend_host_accept_with_secret(&host, &list, 1, "g", &hello_none, [0x33; 32]),
        Err(E2eeError::UnknownPeer)
    ));
    let (_g, hello_wrong) =
        Frl3FriendGuestHandshake::start(&guest, &host_pk, 1, "g", Frl3GuestToken([0xEE; 16]))
            .unwrap();
    assert!(matches!(
        friend_host_accept_with_secret(&host, &list, 1, "g", &hello_wrong, [0x33; 32]),
        Err(E2eeError::UnknownPeer)
    ));
}

// ===========================================================================
// Full loopback: real TcpFriendRelay between a real host tunnel and a real
// Frl3Stream guest — the relay provably forwards only ciphertext.
// ===========================================================================

const HOST_SECRET: [u8; 16] = [0xC7; 16];
const SECRET_CHAT: &[u8] = b"SECRET_WORLD_CHAT_12345";

fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

/// Pump the relay until provably idle (mirrors the FRL2 loopback test).
fn pump(relay: &mut TcpFriendRelay) {
    let mut saw_activity = false;
    let mut quiet = 0;
    for _ in 0..600 {
        if relay.poll_once(Instant::now()).unwrap() == 0 {
            quiet += 1;
            if quiet >= if saw_activity { 3 } else { 20 } {
                return;
            }
        } else {
            saw_activity = true;
            quiet = 0;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// A host-side `FRL2` link speaking raw frames over a blocking socket.
struct HostLink {
    sock: TcpStream,
    inbuf: Vec<u8>,
}

impl HostLink {
    fn register(relay: &mut TcpFriendRelay, addr: SocketAddr) -> (Self, InviteCode) {
        let sock = TcpStream::connect(addr).unwrap();
        sock.set_nodelay(true).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        pump(relay);
        let mut link = Self {
            sock,
            inbuf: Vec::new(),
        };
        match link.recv() {
            FriendPacket::RelayHello { .. } => {}
            other => panic!("expected RelayHello, got {other:?}"),
        }
        link.send(&FriendPacket::HostRegister {
            protocol_version: 767,
            world_name: "E2EE World".to_owned(),
            max_guests: 4,
            session_key: HOST_SECRET,
        });
        pump(relay);
        match link.recv() {
            FriendPacket::HostRegistered { invite_code } => (link, invite_code),
            other => panic!("expected HostRegistered, got {other:?}"),
        }
    }

    fn send(&mut self, packet: &FriendPacket) {
        self.sock.write_all(&encode_frame(packet).unwrap()).unwrap();
    }

    fn recv(&mut self) -> FriendPacket {
        loop {
            if self.inbuf.len() >= 4 {
                let n = u32::from_be_bytes(self.inbuf[..4].try_into().unwrap()) as usize;
                if n > MAX_FRL2_FRAME_BYTES {
                    panic!("oversize frame");
                }
                if self.inbuf.len() >= 4 + n {
                    let body: Vec<u8> = self.inbuf.drain(..4 + n).collect();
                    return decode_packet(&body[4..]).unwrap();
                }
            }
            let mut buf = [0u8; 8192];
            match self.sock.read(&mut buf) {
                Ok(0) => panic!("host link closed"),
                Ok(m) => self.inbuf.extend_from_slice(&buf[..m]),
                Err(e) => panic!("host read: {e}"),
            }
        }
    }
}

/// The relay forwards `StreamData` payloads verbatim — so the bytes the
/// host receives are exactly what an inspecting relay would see. None may
/// contain the plaintext marker.
fn assert_no_plaintext(payloads: &[&[u8]]) {
    for p in payloads {
        assert!(
            !p.windows(SECRET_CHAT.len()).any(|w| w == SECRET_CHAT),
            "relay-visible payload must never contain plaintext"
        );
    }
}

#[test]
fn loopback_psk_e2ee_tunnel_is_opaque_to_relay() {
    let mut relay = TcpFriendRelay::bind(loopback()).unwrap();
    let relay_addr = relay.local_addr().unwrap();
    let (mut host, code) = HostLink::register(&mut relay, relay_addr);

    // The guest dials, joins with the invite code, then runs FRL3-PSK
    // inside StreamData on a real Frl3Stream<TcpStream>.
    let guest_io = TcpStream::connect(relay_addr).unwrap();
    guest_io.set_nodelay(true).unwrap();
    guest_io
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let join = std::thread::spawn(move || Frl3Stream::join(guest_io, &code, "Alice"));
    pump(&mut relay);
    let (stream_id, guest_name) = match host.recv() {
        FriendPacket::StreamOpen {
            stream_id,
            guest_name,
        } => (stream_id, guest_name),
        other => panic!("expected StreamOpen, got {other:?}"),
    };
    assert_eq!(guest_name, "Alice");
    pump(&mut relay);
    // The hello arrives as a StreamData payload — a handshake blob the
    // relay forwards verbatim without understanding.
    let hello_payload = match host.recv() {
        FriendPacket::StreamData { stream_id: s, payload } if s == stream_id => payload,
        other => panic!("expected FRL3 hello payload, got {other:?}"),
    };
    assert!(Frl3Message::is_handshake(&hello_payload));
    let hello = match Frl3Message::decode(&hello_payload).unwrap() {
        Frl3Message::Hello(h) => h,
        other => panic!("expected Frl3Hello, got {other:?}"),
    };
    let (mut host_cipher, ack) =
        frl_e2ee::host_accept(&code, stream_id, &guest_name, &hello).unwrap();
    host.send(&FriendPacket::StreamData {
        stream_id,
        payload: Frl3Message::Ack(ack).encode(),
    });
    pump(&mut relay);
    let mut guest = join
        .join()
        .unwrap()
        .unwrap_or_else(|e| panic!("guest join failed: {e}"));
    assert_eq!(guest.stream_id(), stream_id);
    assert_eq!(guest.sas_code(), host_cipher.sas_code(), "SAS must match");

    // Guest → host: the secret rides as ciphertext records.
    guest.send(SECRET_CHAT).unwrap();
    pump(&mut relay);
    let got = match host.recv() {
        FriendPacket::StreamData { stream_id: s, payload } if s == stream_id => payload,
        other => panic!("expected StreamData, got {other:?}"),
    };
    assert_no_plaintext(&[&hello_payload, &got]);
    assert_eq!(host_cipher.open(&got).unwrap(), SECRET_CHAT);

    // Host → guest: same opacity in the other direction.
    let reply = host_cipher.seal(b"OK: secret received").unwrap();
    host.send(&FriendPacket::StreamData {
        stream_id,
        payload: reply.clone(),
    });
    pump(&mut relay);
    assert_eq!(guest.recv().unwrap(), b"OK: secret received");
    assert_no_plaintext(&[&reply]);
}

#[test]
fn loopback_friend_e2ee_tunnel_and_allowlist_enforcement() {
    let mut relay = TcpFriendRelay::bind(loopback()).unwrap();
    let relay_addr = relay.local_addr().unwrap();
    let (mut host, code) = HostLink::register(&mut relay, relay_addr);

    let host_id = Frl3Identity::generate();
    let guest_id = Frl3Identity::generate();
    let mut allowlist = FriendAllowlist::new();
    allowlist.add(guest_id.public_key());
    let host_pk = host_id.public_key();

    let guest_io = TcpStream::connect(relay_addr).unwrap();
    guest_io.set_nodelay(true).unwrap();
    guest_io
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let guest_id2 = guest_id.clone();
    let join = std::thread::spawn(move || {
        Frl3Stream::join_friend(
            guest_io,
            &code,
            "Alice",
            &guest_id2,
            &host_pk,
            Frl3GuestToken::NONE,
        )
    });
    pump(&mut relay);
    let (stream_id, guest_name) = match host.recv() {
        FriendPacket::StreamOpen {
            stream_id,
            guest_name,
        } => (stream_id, guest_name),
        other => panic!("expected StreamOpen, got {other:?}"),
    };
    pump(&mut relay);
    let hello_payload = match host.recv() {
        FriendPacket::StreamData { stream_id: s, payload } if s == stream_id => payload,
        other => panic!("expected FriendHello payload, got {other:?}"),
    };
    let hello = match Frl3Message::decode(&hello_payload).unwrap() {
        Frl3Message::FriendHello(h) => h,
        other => panic!("expected Frl3FriendHello, got {other:?}"),
    };
    assert_eq!(hello.guest_static_pk, guest_id.public_key());
    let (mut host_cipher, ack) =
        frl_e2ee::friend_host_accept(&host_id, &allowlist, stream_id, &guest_name, &hello).unwrap();
    host.send(&FriendPacket::StreamData {
        stream_id,
        payload: Frl3Message::FriendAck(ack).encode(),
    });
    pump(&mut relay);
    let mut guest = join
        .join()
        .unwrap()
        .unwrap_or_else(|e| panic!("guest join failed: {e}"));
    assert_eq!(guest.sas_code(), host_cipher.sas_code());

    guest.send(SECRET_CHAT).unwrap();
    pump(&mut relay);
    let payload = match host.recv() {
        FriendPacket::StreamData { stream_id: s, payload } if s == stream_id => payload,
        other => panic!("expected StreamData, got {other:?}"),
    };
    assert_no_plaintext(&[&payload]);
    assert_eq!(host_cipher.open(&payload).unwrap(), SECRET_CHAT);
}
