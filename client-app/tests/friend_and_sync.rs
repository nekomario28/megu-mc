// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Live FRL1 loopback relay integration with genuine Minecraft status frames.

use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use megu_client::cli::{help_text, parse_args};
use megu_client::friend_client::{
    FriendGuestBridge, FriendGuestSession, FriendHostEvent, FriendHostSession,
};
use megu_client::protocol::{encode_packet, write_string, write_varint};
use megu_proto::friend_relay::{InviteCode, TcpFriendRelay, MAX_STREAM_PAYLOAD_BYTES};

const TIMEOUT: Duration = Duration::from_secs(5);

struct RelayFixture {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl RelayFixture {
    fn start() -> Self {
        let mut relay = TcpFriendRelay::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = relay.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let running = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !running.load(Ordering::Relaxed) {
                relay.poll_once(Instant::now()).unwrap();
                thread::sleep(Duration::from_millis(1));
            }
        });
        Self {
            addr,
            stop,
            thread: Some(thread),
        }
    }

    fn address(&self) -> String {
        self.addr.to_string()
    }
}

impl Drop for RelayFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn host(relay: &RelayFixture) -> FriendHostSession {
    FriendHostSession::register(&relay.address(), 767, "Shared World", 2, [57; 16], TIMEOUT)
        .unwrap()
}

fn handshake_status() -> Vec<u8> {
    let mut handshake = Vec::new();
    write_varint(&mut handshake, 767);
    write_string(&mut handshake, "friend.example");
    handshake.extend_from_slice(&25565u16.to_be_bytes());
    write_varint(&mut handshake, 1);
    let mut frames = encode_packet(0, &handshake, None);
    frames.extend_from_slice(&encode_packet(0, &[], None));
    frames
}

#[test]
fn real_relay_invite_roundtrip_handshake_and_status() {
    let relay = RelayFixture::start();
    let mut host = host(&relay);
    let canonical = host.invite_code().to_string();
    assert!(canonical.starts_with("MEGU-"));
    assert_eq!(canonical.len(), 14);
    let compact = canonical.replace('-', "").to_ascii_lowercase();
    let code = InviteCode::parse(&compact).unwrap();
    assert_eq!(host.invite_code(), code);

    let mut guest = FriendGuestSession::connect(&relay.address(), &code, "Alex", TIMEOUT).unwrap();
    let stream_id = guest.stream_id();
    assert_eq!(
        host.recv_event().unwrap(),
        FriendHostEvent::GuestOpened {
            stream_id,
            guest_name: "Alex".to_string(),
        }
    );
    let request = handshake_status();
    guest.send_game_bytes(&request).unwrap();
    assert_eq!(
        host.recv_event().unwrap(),
        FriendHostEvent::GameData {
            stream_id,
            payload: request,
        }
    );

    let mut response = Vec::new();
    write_string(
        &mut response,
        r#"{"version":{"name":"1.21.1","protocol":767},"players":{"max":2,"online":1},"description":{"text":"Shared World"}}"#,
    );
    let status = encode_packet(0, &response, None);
    host.send_game_bytes(stream_id, &status).unwrap();
    assert_eq!(guest.recv_game_bytes().unwrap(), status);
    guest.close("finished status").unwrap();
    assert!(matches!(
        host.recv_event().unwrap(),
        FriendHostEvent::GuestClosed { stream_id: id, .. } if id == stream_id
    ));
    host.revoke().unwrap();
}

#[test]
fn relay_rejects_unknown_codes_and_closes_on_revoke() {
    let relay = RelayFixture::start();
    let mut host = host(&relay);
    let mut other = host.invite_code().as_bytes();
    other[0] ^= 0x80;
    let err = FriendGuestSession::connect(
        &relay.address(),
        &InviteCode::from_bytes(other),
        "BadGuest",
        TIMEOUT,
    )
    .err()
    .unwrap();
    assert!(err.contains("unknown invite"), "{err}");

    let mut guest =
        FriendGuestSession::connect(&relay.address(), &host.invite_code(), "ValidGuest", TIMEOUT)
            .unwrap();
    assert!(matches!(
        host.recv_event().unwrap(),
        FriendHostEvent::GuestOpened { .. }
    ));
    host.revoke().unwrap();
    assert!(guest
        .recv_game_bytes()
        .unwrap_err()
        .contains("invite revoked"));
}

#[test]
fn host_tracks_multiple_streams_and_chunks_large_game_payloads() {
    let relay = RelayFixture::start();
    let mut host = host(&relay);
    let mut guest_a =
        FriendGuestSession::connect(&relay.address(), &host.invite_code(), "A", TIMEOUT).unwrap();
    let mut guest_b =
        FriendGuestSession::connect(&relay.address(), &host.invite_code(), "B", TIMEOUT).unwrap();
    let mut ids = Vec::new();
    for _ in 0..2 {
        match host.recv_event().unwrap() {
            FriendHostEvent::GuestOpened { stream_id, .. } => ids.push(stream_id),
            event => panic!("expected stream open: {event:?}"),
        }
    }
    assert_ne!(guest_a.stream_id(), guest_b.stream_id());
    assert!(host.send_game_bytes(u32::MAX, b"foreign").is_err());
    let payload = vec![0xAB; MAX_STREAM_PAYLOAD_BYTES + 123];
    guest_a.send_game_bytes(&payload).unwrap();
    let mut collected = Vec::new();
    while collected.len() < payload.len() {
        match host.recv_event().unwrap() {
            FriendHostEvent::GameData {
                stream_id,
                payload: chunk,
            } => {
                assert_eq!(stream_id, guest_a.stream_id());
                collected.extend(chunk);
            }
            event => panic!("expected game data: {event:?}"),
        }
    }
    assert_eq!(collected, payload);
    host.send_game_bytes(guest_b.stream_id(), b"B-only")
        .unwrap();
    assert_eq!(guest_b.recv_game_bytes().unwrap(), b"B-only");
    guest_a.close("bye").unwrap();
    guest_b.close("bye").unwrap();
    host.revoke().unwrap();
}

#[test]
fn cli_invites_are_validated_and_help_lists_both_flags() {
    let code = InviteCode::from_bytes([1, 2, 3, 4, 5]);
    let relay = "127.0.0.1:23456";
    let cfg = parse_args(&[
        "--join-code",
        &code.to_string().to_ascii_lowercase(),
        "--relay",
        relay,
    ])
    .unwrap();
    assert_eq!(cfg.join_code, Some(code));
    assert_eq!(cfg.relay.as_deref(), Some(relay));
    assert!(cfg.wants_session_action());
    assert!(parse_args(&["--join-code", "BAD", "--relay", relay])
        .unwrap_err()
        .contains("invalid --join-code"));
    assert!(parse_args(&["--join-code", &code.to_string()])
        .unwrap_err()
        .contains("requires --relay"));
    assert!(parse_args(&["--join-code", &code.to_string(), "--relay", "localhost"]).is_err());
    assert!(help_text().contains("--join-code <MEGU-XXXX-XXXX>"));
    assert!(help_text().contains("--relay <host:port>"));
}

#[test]
fn host_debug_redacts_credential() {
    let relay = RelayFixture::start();
    let host = host(&relay);
    let debug = format!("{host:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("[57, 57"));
    assert!(!debug.contains("9999999999999999"));
}

#[test]
fn loopback_bridge_passes_real_status_bytes() {
    use std::io::{Read, Write};
    let relay = RelayFixture::start();
    let mut host = host(&relay);
    let bridge = FriendGuestBridge::start(
        &relay.address(),
        &host.invite_code(),
        "BridgeGuest",
        TIMEOUT,
    )
    .unwrap();
    let mut local = TcpStream::connect(bridge.local_addr()).unwrap();
    local.set_read_timeout(Some(TIMEOUT)).unwrap();
    let stream_id = match host.recv_event().unwrap() {
        FriendHostEvent::GuestOpened { stream_id, .. } => stream_id,
        event => panic!("expected bridge guest: {event:?}"),
    };
    let request = handshake_status();
    local.write_all(&request).unwrap();
    assert_eq!(
        host.recv_event().unwrap(),
        FriendHostEvent::GameData {
            stream_id,
            payload: request
        }
    );
    let response = encode_packet(0, b"status", None);
    host.send_game_bytes(stream_id, &response).unwrap();
    let mut got = vec![0; response.len()];
    local.read_exact(&mut got).unwrap();
    assert_eq!(got, response);
    drop(local);
    drop(bridge);
    host.revoke().unwrap();
}
