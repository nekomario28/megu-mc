// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Real loopback proof for [`mc_spine_proto::friend_relay`]: one
//! [`TcpFriendRelay`] on an ephemeral `127.0.0.1:0` port, one real host
//! tunnel and multiple real guest links exercise the whole `FRL1` path —
//! `MEGU-XXXX-XXXX` invite issuance, per-stream multiplexing of genuine
//! protocol-767 Minecraft frames in both directions with zero cross-stream
//! leakage, the `max_guests` cap, unknown-code and wrong-secret rejections,
//! and a valid `HostRevoke` retiring the code and pushing `StreamClose` to
//! every live guest — all over live kernel TCP with no stubs.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use mc_spine_proto::friend_relay::{
    decode_packet, encode_frame, FriendPacket, InviteCode, TcpFriendRelay,
    MAX_FRL1_FRAME_BYTES,
};
use mc_spine_proto::wire::{write_frame, WireWriter};

const HOST_SECRET: [u8; 16] = [0xC7; 16];

fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

/// One test "peer": a real blocking `TcpStream` speaking `FRL1` frames.
struct Link {
    sock: TcpStream,
}

impl Link {
    fn dial(relay_addr: SocketAddr) -> Self {
        let sock = TcpStream::connect(relay_addr).unwrap();
        sock.set_nodelay(true).unwrap();
        Self { sock }
    }

    fn send(&mut self, packet: &FriendPacket) {
        self.sock.write_all(&encode_frame(packet).unwrap()).unwrap();
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        self.sock.write_all(bytes).unwrap();
    }

    /// Receive and decode one frame body, or `None` after `ms`.
    fn recv(&mut self, ms: u64) -> Option<FriendPacket> {
        self.sock
            .set_read_timeout(Some(Duration::from_millis(ms)))
            .unwrap();
        let mut len = [0u8; 4];
        if self.sock.read_exact(&mut len).is_err() {
            return None;
        }
        let n = u32::from_be_bytes(len) as usize;
        if n > MAX_FRL1_FRAME_BYTES {
            return None;
        }
        let mut body = vec![0u8; n];
        if self.sock.read_exact(&mut body).is_err() {
            return None;
        }
        decode_packet(&body).ok()
    }

    /// Fail unless the link stays silent for `ms`.
    fn expect_silent(&mut self, ms: u64) {
        if let Some(p) = self.recv(ms) {
            panic!("expected silence on the link, received {p:?}");
        }
    }

    /// `true` iff the peer closed the link within `ms`.
    fn wait_eof(&mut self, ms: u64) -> bool {
        self.sock
            .set_read_timeout(Some(Duration::from_millis(ms)))
            .unwrap();
        let mut b = [0u8; 1];
        match self.sock.read(&mut b) {
            Ok(0) => true,
            Ok(_) => false,
            // A read timeout means the link is alive and silent — not closed.
            Err(e) => !matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ),
        }
    }
}

/// Pump the relay until it is provably idle: a burst of loopback traffic
/// settles in microseconds, so three consecutive empty polls (or twenty
/// when nothing ever arrives) is a settled queue, not a lucky guess.
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

/// Register a host tunnel and return `(link, invite_code)`.
fn register_host(relay: &mut TcpFriendRelay, addr: SocketAddr, max_guests: u8) -> (Link, InviteCode) {
    let mut host = Link::dial(addr);
    host.send(&FriendPacket::HostRegister {
        protocol_version: 767,
        world_name: "Loopback LAN".to_owned(),
        max_guests,
        host_secret: HOST_SECRET,
    });
    pump(relay);
    match host.recv(2000) {
        Some(FriendPacket::HostRegistered { invite_code }) => (host, invite_code),
        other => panic!("expected HostRegistered, got {other:?}"),
    }
}

/// Join a guest and return `(link, stream_id)`; the host's `StreamOpen` is
/// left in its socket for the caller to read.
fn join_guest(
    relay: &mut TcpFriendRelay,
    addr: SocketAddr,
    code: InviteCode,
    name: &str,
) -> (Link, u32) {
    let mut guest = Link::dial(addr);
    guest.send(&FriendPacket::GuestJoin {
        invite_code: code,
        guest_name: name.to_owned(),
    });
    pump(relay);
    match guest.recv(2000) {
        Some(FriendPacket::GuestAccepted { stream_id }) => (guest, stream_id),
        other => panic!("expected GuestAccepted for {name}, got {other:?}"),
    }
}

/// A rejected `GuestJoin` reads its `GuestRejected` reason, then the link
/// must close.
fn join_expect_reject(
    relay: &mut TcpFriendRelay,
    addr: SocketAddr,
    code: InviteCode,
    name: &str,
) -> String {
    let mut guest = Link::dial(addr);
    guest.send(&FriendPacket::GuestJoin {
        invite_code: code,
        guest_name: name.to_owned(),
    });
    pump(relay);
    let reason = match guest.recv(2000) {
        Some(FriendPacket::GuestRejected { reason }) => reason,
        other => panic!("expected GuestRejected for {name}, got {other:?}"),
    };
    assert!(guest.wait_eof(2000), "rejected link must be closed");
    reason
}

// ---- Genuine Minecraft protocol-767 frames, carried as opaque payloads ----

/// Handshake(id 0x00): protocol 767, address, port, next-state 1 (status).
fn handshake_767() -> Vec<u8> {
    let mut p = WireWriter::new();
    p.write_varint(767);
    p.write_string("127.0.0.1");
    p.write_u16(25565);
    p.write_varint(1);
    let mut frame = Vec::new();
    write_frame(&mut frame, 0x00, p.as_slice());
    frame
}

/// Status request(id 0x00): empty body.
fn status_request() -> Vec<u8> {
    let mut frame = Vec::new();
    write_frame(&mut frame, 0x00, &[]);
    frame
}

/// Status response(id 0x00): the MOTD JSON document.
fn status_response() -> Vec<u8> {
    let mut p = WireWriter::new();
    p.write_string(
        r#"{"version":{"name":"1.21.1","protocol":767},"players":{"max":8,"online":3}}"#,
    );
    let mut frame = Vec::new();
    write_frame(&mut frame, 0x00, p.as_slice());
    frame
}

/// A play-state keepalive(id 0x18 in 767's C2S): one big-endian i64.
fn play_keepalive() -> Vec<u8> {
    let mut p = WireWriter::new();
    p.write_i64(0x0123_4567_89AB_CDEF);
    let mut frame = Vec::new();
    write_frame(&mut frame, 0x18, p.as_slice());
    frame
}

/// Assert the next frame on `link` is `StreamData` for `stream_id` carrying
/// exactly `payload` — the verbatim-tunnel contract.
fn expect_data(link: &mut Link, stream_id: u32, payload: &[u8]) {
    match link.recv(2000) {
        Some(FriendPacket::StreamData {
            stream_id: got,
            payload: got_payload,
        }) => {
            assert_eq!(got, stream_id, "StreamData stream id");
            assert_eq!(got_payload, payload, "StreamData payload must be verbatim");
        }
        other => panic!("expected StreamData on stream {stream_id}, got {other:?}"),
    }
}

#[test]
fn register_two_guests_bridge_frames_cap_and_revoke() {
    let mut relay = TcpFriendRelay::bind(loopback()).unwrap();
    let relay_addr = relay.local_addr().unwrap();

    // The host opens its world for two friends and gets one invite code.
    let (mut host, code) = register_host(&mut relay, relay_addr, 2);
    let text = code.to_string();
    assert!(text.starts_with("MEGU-"), "invite renders MEGU-XXXX-XXXX");
    assert_eq!(InviteCode::parse(&text).unwrap(), code);
    assert_eq!(relay.metrics().active_hosts, 1);

    // Guests 1 and 2 join over real TCP with the code; the host is told
    // each stream opens, with the join name.
    let (mut g1, s1) = join_guest(&mut relay, relay_addr, code, "Alice");
    match host.recv(2000) {
        Some(FriendPacket::StreamOpen {
            stream_id,
            guest_name,
        }) => {
            assert_eq!(stream_id, s1);
            assert_eq!(guest_name, "Alice");
        }
        other => panic!("expected StreamOpen for Alice, got {other:?}"),
    }
    let (mut g2, s2) = join_guest(&mut relay, relay_addr, code, "Bob");
    match host.recv(2000) {
        Some(FriendPacket::StreamOpen {
            stream_id,
            guest_name,
        }) => {
            assert_eq!(stream_id, s2);
            assert_eq!(guest_name, "Bob");
        }
        other => panic!("expected StreamOpen for Bob, got {other:?}"),
    }
    assert_ne!(s1, s2);
    assert_eq!(relay.metrics().active_guests, 2);

    // Guest 3 hits the cap; a well-formed but unknown code fails too.
    assert_eq!(
        join_expect_reject(&mut relay, relay_addr, code, "Carol"),
        "world is full"
    );
    let mut wrong_bytes = code.as_bytes();
    wrong_bytes[0] ^= 0xFF;
    assert_eq!(
        join_expect_reject(&mut relay, relay_addr, InviteCode::from_bytes(wrong_bytes), "Dave"),
        "unknown invite"
    );

    // Bidirectional bridge: Alice's real 767 handshake rides her stream to
    // the host verbatim; the host's status response rides back the same
    // way.
    let hs = handshake_767();
    g1.send(&FriendPacket::StreamData {
        stream_id: s1,
        payload: hs.clone(),
    });
    pump(&mut relay);
    expect_data(&mut host, s1, &hs);

    let sr = status_response();
    host.send(&FriendPacket::StreamData {
        stream_id: s1,
        payload: sr.clone(),
    });
    pump(&mut relay);
    expect_data(&mut g1, s1, &sr);

    // Bob's stream carries its own real frames — a status request upstream
    // and a play keepalive downstream — and Alice's socket stays silent:
    // zero cross-stream leakage.
    let sq = status_request();
    g2.send(&FriendPacket::StreamData {
        stream_id: s2,
        payload: sq.clone(),
    });
    pump(&mut relay);
    expect_data(&mut host, s2, &sq);
    let pk = play_keepalive();
    host.send(&FriendPacket::StreamData {
        stream_id: s2,
        payload: pk.clone(),
    });
    pump(&mut relay);
    expect_data(&mut g2, s2, &pk);
    g1.expect_silent(150);

    // Liveness on both link kinds.
    host.send(&FriendPacket::Heartbeat { nonce: 7 });
    g1.send(&FriendPacket::Heartbeat { nonce: 8 });
    pump(&mut relay);
    assert_eq!(
        host.recv(2000),
        Some(FriendPacket::HeartbeatAck { nonce: 7 })
    );
    assert_eq!(
        g1.recv(2000),
        Some(FriendPacket::HeartbeatAck { nonce: 8 })
    );

    // A wrong host secret revokes nothing: `ok: false`, the code lives on
    // and bytes still flow.
    host.send(&FriendPacket::HostRevoke {
        invite_code: code,
        host_secret: [0xEE; 16],
    });
    pump(&mut relay);
    assert_eq!(host.recv(2000), Some(FriendPacket::HostRevoked { ok: false }));
    assert_eq!(relay.metrics().revoked_codes, 0);
    host.send(&FriendPacket::StreamData {
        stream_id: s1,
        payload: pk.clone(),
    });
    pump(&mut relay);
    expect_data(&mut g1, s1, &pk);

    // The right secret retires the session: the code dies, both live
    // guests are told `StreamClose`, and a late join fails closed.
    host.send(&FriendPacket::HostRevoke {
        invite_code: code,
        host_secret: HOST_SECRET,
    });
    pump(&mut relay);
    assert_eq!(host.recv(2000), Some(FriendPacket::HostRevoked { ok: true }));
    for (guest, stream_id) in [(&mut g1, s1), (&mut g2, s2)] {
        match guest.recv(2000) {
            Some(FriendPacket::StreamClose {
                stream_id: got, ..
            }) => assert_eq!(got, stream_id),
            other => panic!("expected StreamClose on stream {stream_id}, got {other:?}"),
        }
    }
    assert_eq!(
        join_expect_reject(&mut relay, relay_addr, code, "Late"),
        "unknown invite"
    );

    // Exact wire accounting for everything above.
    let m = relay.metrics();
    assert_eq!(m.active_hosts, 0, "revoked session is gone");
    assert_eq!(m.active_guests, 0, "both streams closed with the session");
    assert_eq!(
        m.rejected_joins, 3,
        "cap + unknown code + post-revoke join"
    );
    assert_eq!(m.revoked_codes, 1);
    assert_eq!(
        m.bytes_relayed,
        (hs.len() + sr.len() + sq.len() + pk.len() + pk.len()) as u64,
        "every forwarded payload byte, both directions"
    );
}

#[test]
fn foreign_stream_ids_and_garbage_fail_closed() {
    let mut relay = TcpFriendRelay::bind(loopback()).unwrap();
    let relay_addr = relay.local_addr().unwrap();
    let (mut host, code) = register_host(&mut relay, relay_addr, 2);
    let (mut g1, s1) = join_guest(&mut relay, relay_addr, code, "Alice");
    assert!(matches!(
        host.recv(2000),
        Some(FriendPacket::StreamOpen { stream_id, .. }) if stream_id == s1
    ));
    let (mut g2, s2) = join_guest(&mut relay, relay_addr, code, "Bob");
    assert!(matches!(
        host.recv(2000),
        Some(FriendPacket::StreamOpen { stream_id, .. }) if stream_id == s2
    ));

    // Alice forges Bob's stream id: the link dies at the next reap — a
    // guest can never inject bytes into another guest's stream, and Bob's
    // stream is untouched.
    g1.send(&FriendPacket::StreamData {
        stream_id: s2,
        payload: b"forged-by-alice".to_vec(),
    });
    pump(&mut relay);
    assert!(g1.wait_eof(2000), "foreign stream id must kill the link");
    g2.expect_silent(150);
    assert_eq!(relay.metrics().active_guests, 1);
    // The host learns Alice's stream closed.
    match host.recv(2000) {
        Some(FriendPacket::StreamClose { stream_id, .. }) => assert_eq!(stream_id, s1),
        other => panic!("expected StreamClose for Alice's stream, got {other:?}"),
    }

    // Garbage on a fresh link — a bogus length prefix, foreign magic —
    // kills it silently; the relay keeps serving the survivors.
    let mut junk = Link::dial(relay_addr);
    junk.send_raw(&u32::MAX.to_be_bytes());
    pump(&mut relay);
    assert!(junk.wait_eof(2000));
    let mut junk2 = Link::dial(relay_addr);
    junk2.send_raw(b"\x00\x00\x00\x09NOPEnope!");
    pump(&mut relay);
    assert!(junk2.wait_eof(2000));

    // Bob's stream still bridges fine.
    let pk = play_keepalive();
    g2.send(&FriendPacket::StreamData {
        stream_id: s2,
        payload: pk.clone(),
    });
    pump(&mut relay);
    expect_data(&mut host, s2, &pk);

    // A host-side frame naming a foreign stream id is an isolation
    // violation too: the tunnel dies and its session is revoked, which
    // closes Bob's link with a `StreamClose`.
    host.send(&FriendPacket::StreamData {
        stream_id: s2 + 100_000,
        payload: b"not-mine".to_vec(),
    });
    pump(&mut relay);
    assert!(host.wait_eof(2000), "host tunnel must die on a foreign stream id");
    match g2.recv(2000) {
        Some(FriendPacket::StreamClose { stream_id, .. }) => assert_eq!(stream_id, s2),
        other => panic!("expected StreamClose for Bob after host death, got {other:?}"),
    }
    assert_eq!(relay.metrics().active_hosts, 0);
    assert_eq!(relay.metrics().active_guests, 0);
    assert_eq!(
        join_expect_reject(&mut relay, relay_addr, code, "Late"),
        "unknown invite"
    );
}

#[test]
fn guest_stream_close_reaches_the_host() {
    let mut relay = TcpFriendRelay::bind(loopback()).unwrap();
    let relay_addr = relay.local_addr().unwrap();
    let (mut host, code) = register_host(&mut relay, relay_addr, 1);
    let (mut g1, s1) = join_guest(&mut relay, relay_addr, code, "Alice");
    assert!(matches!(
        host.recv(2000),
        Some(FriendPacket::StreamOpen { stream_id, .. }) if stream_id == s1
    ));

    // The guest ends its own stream; the host sees the close, the slot
    // frees, and a fresh guest can take it.
    g1.send(&FriendPacket::StreamClose {
        stream_id: s1,
        reason: "guest quit".to_owned(),
    });
    pump(&mut relay);
    assert!(g1.wait_eof(2000));
    match host.recv(2000) {
        Some(FriendPacket::StreamClose { stream_id, .. }) => assert_eq!(stream_id, s1),
        other => panic!("expected StreamClose for s1, got {other:?}"),
    }
    assert_eq!(relay.metrics().active_guests, 0);

    let (mut g2, s2) = join_guest(&mut relay, relay_addr, code, "Bob");
    assert!(matches!(
        host.recv(2000),
        Some(FriendPacket::StreamOpen { stream_id, .. }) if stream_id != s1
    ));
    let pk = play_keepalive();
    g2.send(&FriendPacket::StreamData {
        stream_id: s2,
        payload: pk.clone(),
    });
    pump(&mut relay);
    expect_data(&mut host, s2, &pk);
}
