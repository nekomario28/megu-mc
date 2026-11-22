// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Real loopback proof for [`megu_proto::voice_relay`]: one
//! [`UdpVoiceRelay`] on an ephemeral `127.0.0.1:0` port plus four real UDP
//! "client" sockets exercise the whole `VCR1` path — secret-token auth
//! (wrong secrets fail closed), proximity fan-out with exact squared
//! distance, the whisper radius, dimension culling, an `Isolated` group that
//! hears across dimensions while nearby outsiders hear nothing, and
//! mute/deafen/block plus malformed/oversized/rate-limited drops, all over
//! live kernel UDP with no stubs.

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use megu_proto::voice_relay::{
    decode_packet, encode_packet, UdpVoiceRelay, VoiceGroupKind, VoicePacket, VoicePlayerState,
    VoiceRelayConfig, VoiceRelayState,
};

const OVERWORLD: &str = "minecraft:overworld";
const NETHER: &str = "minecraft:the_nether";

/// One test "voice client": a real bound UDP socket plus the identity the
/// relay was told to expect.
struct Client {
    sock: UdpSocket,
    uuid: [u8; 16],
    secret: [u8; 16],
}

impl Client {
    fn bind(uuid_byte: u8, secret_byte: u8) -> Self {
        Self {
            sock: UdpSocket::bind("127.0.0.1:0").unwrap(),
            uuid: [uuid_byte; 16],
            secret: [secret_byte; 16],
        }
    }

    fn send(&self, relay: SocketAddr, packet: &VoicePacket) {
        self.sock
            .send_to(&encode_packet(packet).unwrap(), relay)
            .unwrap();
    }

    fn send_raw(&self, relay: SocketAddr, bytes: &[u8]) {
        self.sock.send_to(bytes, relay).unwrap();
    }

    /// Receive and decode one datagram, or `None` after `ms`.
    fn recv(&self, ms: u64) -> Option<VoicePacket> {
        self.sock
            .set_read_timeout(Some(Duration::from_millis(ms)))
            .unwrap();
        let mut buf = [0u8; 2048];
        match self.sock.recv_from(&mut buf) {
            Ok((n, _)) => decode_packet(&buf[..n]).ok(),
            Err(_) => None,
        }
    }

    /// Fail unless the socket stays silent for `ms`.
    fn expect_silent(&self, ms: u64) {
        if let Some(p) = self.recv(ms) {
            panic!("expected silence on the socket, received {p:?}");
        }
    }
}

/// Register `c` with the relay the way the game layer would: identity,
/// secret, current dimension and position.
fn register(relay: &mut UdpVoiceRelay, c: &Client, dimension: &str, pos: [f64; 3]) {
    let mut p = VoicePlayerState::new(c.uuid, c.secret);
    p.dimension = dimension.to_owned();
    p.pos = pos;
    relay.state_mut().upsert_player(p);
}

/// Drain the relay once it has at least one datagram (loopback delivery is
/// asynchronous by a few microseconds; poll a bounded number of times
/// rather than assuming one pass catches everything).
fn pump(relay: &mut UdpVoiceRelay) {
    for _ in 0..100 {
        if relay.poll_once(Instant::now()).unwrap() > 0 {
            // One more pass for stragglers queued behind the first drain.
            relay.poll_once(Instant::now()).unwrap();
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn auth(relay: &mut UdpVoiceRelay, relay_addr: SocketAddr, c: &Client) {
    c.send(
        relay_addr,
        &VoicePacket::Auth {
            player_uuid: c.uuid,
            secret: c.secret,
        },
    );
    pump(relay);
    match c.recv(2000) {
        Some(VoicePacket::AuthAck { ok: true }) => {}
        other => panic!("expected AuthAck ok for {:02x?}, got {other:?}", c.uuid),
    }
}

fn mic(c: &Client, relay: SocketAddr, seq: u64, whispering: bool, opus: &[u8]) {
    c.send(
        relay,
        &VoicePacket::MicFrame {
            player_uuid: c.uuid,
            seq,
            whispering,
            opus_payload: opus.to_vec(),
        },
    );
}

/// Assert the next datagram on `c` is the `PlayerSound` for `seq` from
/// `sender` with exactly `opus`, returning `(distance_sq, group)` for the
/// caller to check the channel.
fn expect_sound(c: &Client, sender: [u8; 16], seq: u64, opus: &[u8]) -> (f32, bool) {
    match c.recv(2000) {
        Some(VoicePacket::PlayerSound {
            sender_uuid,
            seq: got_seq,
            whispering: _,
            distance_sq,
            group,
            opus_payload,
        }) => {
            assert_eq!(sender_uuid, sender, "PlayerSound sender");
            assert_eq!(got_seq, seq, "PlayerSound seq");
            assert_eq!(opus_payload, opus, "PlayerSound payload must be verbatim");
            (distance_sq, group)
        }
        other => panic!("expected PlayerSound seq {seq}, got {other:?}"),
    }
}

#[test]
fn auth_proximity_whisper_and_isolated_group_over_udp() {
    let mut relay = UdpVoiceRelay::bind(
        SocketAddr::from(([127, 0, 0, 1], 0)),
        VoiceRelayState::new(VoiceRelayConfig::default()),
    )
    .unwrap();
    let relay_addr = relay.local_addr().unwrap();

    let alice = Client::bind(0xA1, 0x11);
    let bob = Client::bind(0xB0, 0x22);
    let charlie = Client::bind(0xC0, 0x33);
    let dave = Client::bind(0xD0, 0x44);

    register(&mut relay, &alice, OVERWORLD, [0.0, 64.0, 0.0]);
    register(&mut relay, &bob, OVERWORLD, [10.0, 64.0, 0.0]);
    register(&mut relay, &charlie, OVERWORLD, [80.0, 64.0, 0.0]);
    register(&mut relay, &dave, NETHER, [5.0, 64.0, 0.0]);

    // Wrong secret fails closed: rejected AuthAck, no binding, and a
    // MicFrame from the still-unauthenticated address is never relayed.
    bob.send(
        relay_addr,
        &VoicePacket::Auth {
            player_uuid: bob.uuid,
            secret: [0xEE; 16],
        },
    );
    pump(&mut relay);
    assert_eq!(bob.recv(2000), Some(VoicePacket::AuthAck { ok: false }));
    assert_eq!(relay.metrics().auth_rejected, 1);
    mic(&bob, relay_addr, 0, false, b"unauthenticated-spoof");
    pump(&mut relay);
    alice.expect_silent(120);
    charlie.expect_silent(120);
    dave.expect_silent(120);

    for c in [&alice, &bob, &charlie, &dave] {
        auth(&mut relay, relay_addr, c);
    }
    assert_eq!(relay.metrics().auth_ok, 4);

    // KeepAlive round-trips its nonce for the authenticated binding.
    alice.send(
        relay_addr,
        &VoicePacket::KeepAlive {
            player_uuid: alice.uuid,
            nonce: 0xDEAD_BEEF,
        },
    );
    pump(&mut relay);
    assert_eq!(
        alice.recv(2000),
        Some(VoicePacket::KeepAliveAck { nonce: 0xDEAD_BEEF })
    );

    // Alice speaks at normal volume: Bob (10m, same dimension) hears the
    // exact Opus payload with the exact squared distance; Charlie (80m >
    // 48m) and Dave (5m but the Nether) hear nothing.
    let opus = b"opus-frame-alice-0001";
    mic(&alice, relay_addr, 1, false, opus);
    pump(&mut relay);
    let (distance_sq, group) = expect_sound(&bob, alice.uuid, 1, opus);
    assert_eq!(distance_sq, 100.0);
    assert!(!group);
    charlie.expect_silent(150);
    dave.expect_silent(150);

    // Alice whispers with Bob at 25m: beyond the 16m whisper radius, so
    // nobody hears it — while normal speech at the same 25m still reaches
    // him, proving the radius (not distance alone) gated delivery.
    relay.state_mut().player_mut(&bob.uuid).unwrap().pos = [25.0, 64.0, 0.0];
    mic(&alice, relay_addr, 2, true, b"whispered-secret");
    pump(&mut relay);
    bob.expect_silent(150);
    charlie.expect_silent(150);
    dave.expect_silent(150);
    mic(&alice, relay_addr, 3, false, b"still-audible-at-25m");
    pump(&mut relay);
    let (distance_sq, group) = expect_sound(&bob, alice.uuid, 3, b"still-audible-at-25m");
    assert_eq!(distance_sq, 625.0);
    assert!(!group);
    charlie.expect_silent(150);
    dave.expect_silent(150);

    // Alice and Dave join an Isolated group: Dave hears Alice across
    // dimensions on the group channel, while Bob — 25m away in the same
    // dimension — hears nothing because an isolated sender leaks no
    // proximity audio.
    relay
        .state_mut()
        .set_group_kind(7, VoiceGroupKind::Isolated);
    assert!(relay.state_mut().join_group(&alice.uuid, 7));
    assert!(relay.state_mut().join_group(&dave.uuid, 7));
    mic(&alice, relay_addr, 4, false, b"group-only-frame");
    pump(&mut relay);
    let (_, group) = expect_sound(&dave, alice.uuid, 4, b"group-only-frame");
    assert!(group);
    bob.expect_silent(150);
    charlie.expect_silent(150);

    // Exact wire accounting for everything above.
    let m = relay.metrics();
    assert_eq!(m.rx_packets, 11, "5 auth + 1 spoof + 1 keepalive + 4 mic");
    assert_eq!(m.tx_packets, 9, "5 authack + 1 keepaliveack + 3 sounds");
    assert_eq!(m.dropped_malformed, 0);
    assert_eq!(m.dropped_rate_limited, 0);
}

#[test]
fn mute_deafen_and_block_suppress_delivery() {
    let mut relay = UdpVoiceRelay::bind(
        SocketAddr::from(([127, 0, 0, 1], 0)),
        VoiceRelayState::new(VoiceRelayConfig::default()),
    )
    .unwrap();
    let relay_addr = relay.local_addr().unwrap();
    let alice = Client::bind(0xA1, 0x11);
    let bob = Client::bind(0xB0, 0x22);
    register(&mut relay, &alice, OVERWORLD, [0.0, 64.0, 0.0]);
    register(&mut relay, &bob, OVERWORLD, [10.0, 64.0, 0.0]);
    auth(&mut relay, relay_addr, &alice);
    auth(&mut relay, relay_addr, &bob);

    // Baseline: Bob hears Alice.
    mic(&alice, relay_addr, 1, false, b"baseline");
    pump(&mut relay);
    expect_sound(&bob, alice.uuid, 1, b"baseline");

    // A muted sender produces no output at all.
    relay.state_mut().player_mut(&alice.uuid).unwrap().muted = true;
    mic(&alice, relay_addr, 2, false, b"muted");
    pump(&mut relay);
    bob.expect_silent(150);

    // A deafened recipient is skipped.
    relay.state_mut().player_mut(&alice.uuid).unwrap().muted = false;
    relay.state_mut().player_mut(&bob.uuid).unwrap().deafened = true;
    mic(&alice, relay_addr, 3, false, b"deafened");
    pump(&mut relay);
    bob.expect_silent(150);

    // A recipient who blocked the sender is skipped.
    relay.state_mut().player_mut(&bob.uuid).unwrap().deafened = false;
    relay
        .state_mut()
        .player_mut(&bob.uuid)
        .unwrap()
        .blocked
        .insert(alice.uuid);
    mic(&alice, relay_addr, 4, false, b"blocked");
    pump(&mut relay);
    bob.expect_silent(150);

    // Unblocking restores delivery end to end.
    relay
        .state_mut()
        .player_mut(&bob.uuid)
        .unwrap()
        .blocked
        .remove(&alice.uuid);
    mic(&alice, relay_addr, 5, false, b"heard-again");
    pump(&mut relay);
    expect_sound(&bob, alice.uuid, 5, b"heard-again");

    let m = relay.metrics();
    assert_eq!(m.rx_packets, 7, "2 auth + 5 mic");
    assert_eq!(m.tx_packets, 4, "2 authack + 2 sounds");
}

#[test]
fn malformed_oversized_and_flooding_datagrams_fail_closed() {
    let mut relay = UdpVoiceRelay::bind(
        SocketAddr::from(([127, 0, 0, 1], 0)),
        VoiceRelayState::new(VoiceRelayConfig {
            max_packets_per_window: 3,
            ..VoiceRelayConfig::default()
        }),
    )
    .unwrap();
    let relay_addr = relay.local_addr().unwrap();
    let alice = Client::bind(0xA1, 0x11);
    let bob = Client::bind(0xB0, 0x22);
    register(&mut relay, &alice, OVERWORLD, [0.0, 64.0, 0.0]);
    register(&mut relay, &bob, OVERWORLD, [10.0, 64.0, 0.0]);
    auth(&mut relay, relay_addr, &alice);
    auth(&mut relay, relay_addr, &bob);

    // Foreign magic, a truncated header, a datagram over the MTU cap and a
    // relay-bound packet type from a client: all dropped, none answered,
    // none relayed — and the relay keeps running.
    alice.send_raw(relay_addr, b"NOPE-not-a-voice-datagram");
    alice.send_raw(relay_addr, b"VCR1");
    alice.send_raw(relay_addr, &vec![0xAB; 1500]);
    alice.send(relay_addr, &VoicePacket::AuthAck { ok: true });
    pump(&mut relay);
    assert_eq!(relay.metrics().dropped_malformed, 4);
    alice.expect_silent(150);
    bob.expect_silent(150);

    // A MicFrame burst over the per-sender window cap: 3 forwarded, the
    // rest dropped at the source so a flooder cannot amplify traffic.
    for seq in 0..8u64 {
        mic(&alice, relay_addr, seq, false, b"flood");
    }
    pump(&mut relay);
    for seq in 0..3u64 {
        expect_sound(&bob, alice.uuid, seq, b"flood");
    }
    bob.expect_silent(150);
    assert_eq!(relay.metrics().dropped_rate_limited, 5);

    let m = relay.metrics();
    assert_eq!(m.rx_packets, 14, "2 auth + 4 malformed + 8 mic");
    assert_eq!(m.tx_packets, 5, "2 authack + 3 sounds");
}
