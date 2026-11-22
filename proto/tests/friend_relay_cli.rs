// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! End-to-end tests for the `mc-spine-friend-relay` binary: real process
//! invocations for usage/security exits, plus a live spawned relay proving
//! host registration, the edge host/guest caps, and bidirectional FRL1
//! StreamData forwarding over kernel TCP.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use mc_spine_proto::friend_relay::{
    decode_packet, encode_frame, FriendPacket, InviteCode, MAX_FRL1_FRAME_BYTES,
};

const BIN: &str = env!("CARGO_BIN_EXE_mc-spine-friend-relay");
const HOST_SECRET: [u8; 16] = [0x5E; 16];

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {BIN}: {e}"))
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

struct ChildGuard {
    child: Child,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Link {
    sock: TcpStream,
}

impl Link {
    fn connect(addr: SocketAddr) -> io::Result<Self> {
        let sock = TcpStream::connect_timeout(&addr, Duration::from_millis(500))?;
        sock.set_nodelay(true)?;
        sock.set_read_timeout(Some(Duration::from_secs(3)))?;
        sock.set_write_timeout(Some(Duration::from_secs(3)))?;
        Ok(Self { sock })
    }

    fn send(&mut self, packet: &FriendPacket) -> io::Result<()> {
        let frame = encode_frame(packet).map_err(io::Error::other)?;
        self.sock.write_all(&frame)
    }

    fn recv(&mut self) -> io::Result<FriendPacket> {
        let mut len = [0u8; 4];
        self.sock.read_exact(&mut len)?;
        let n = u32::from_be_bytes(len) as usize;
        if !(5..=MAX_FRL1_FRAME_BYTES).contains(&n) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("test frame length {n} outside bounds"),
            ));
        }
        let mut body = vec![0u8; n];
        self.sock.read_exact(&mut body)?;
        decode_packet(&body).map_err(io::Error::other)
    }

    fn expect_closed(&mut self) -> bool {
        let mut byte = [0u8; 1];
        match self.sock.read(&mut byte) {
            Ok(0) => true,
            Ok(_) => false,
            Err(e) => !matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ),
        }
    }
}

fn loopback_addr() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

fn wait_for_listener(port: u16) -> Link {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    for _ in 0..100 {
        if let Ok(link) = Link::connect(addr) {
            return link;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("spawned relay did not accept on 127.0.0.1:{port}");
}

fn register_host(link: &mut Link, max_guests: u8) -> InviteCode {
    link.send(&FriendPacket::HostRegister {
        protocol_version: 767,
        world_name: "CLI Test World".to_owned(),
        max_guests,
        host_secret: HOST_SECRET,
    })
    .unwrap();
    match link.recv().unwrap() {
        FriendPacket::HostRegistered { invite_code } => invite_code,
        other => panic!("expected HostRegistered, got {other:?}"),
    }
}

fn join_guest(link: &mut Link, code: InviteCode, name: &str) -> Result<u32, String> {
    link.send(&FriendPacket::GuestJoin {
        invite_code: code,
        guest_name: name.to_owned(),
    })
    .unwrap();
    match link.recv().unwrap() {
        FriendPacket::GuestAccepted { stream_id } => Ok(stream_id),
        FriendPacket::GuestRejected { reason } => Err(reason),
        other => panic!("expected guest join response, got {other:?}"),
    }
}

fn expect_stream_data(link: &mut Link, stream_id: u32, payload: &[u8]) {
    match link.recv().unwrap() {
        FriendPacket::StreamData {
            stream_id: got,
            payload: got_payload,
        } => {
            assert_eq!(got, stream_id);
            assert_eq!(got_payload, payload);
        }
        other => panic!("expected StreamData on stream {stream_id}, got {other:?}"),
    }
}

#[test]
fn help_exits_zero() {
    for flag in ["--help", "-h"] {
        let out = run(&[flag]);
        assert_eq!(code(&out), 0, "{flag}: {}", stderr(&out));
        let text = stdout(&out);
        for needle in [
            "--bind",
            "--public-bind",
            "--max-hosts",
            "--max-guests-per-host",
            "--ttl-secs",
            "--self-test",
        ] {
            assert!(text.contains(needle), "help lacks {needle}:\n{text}");
        }
    }
}

#[test]
fn bad_args_exit_two() {
    for args in [
        vec!["--no-such-option"],
        vec!["positional"],
        vec!["--bind"],
        vec!["--bind", "not-an-addr"],
        vec!["--max-hosts", "0"],
        vec!["--max-hosts", "banana"],
        vec!["--max-guests-per-host", "0"],
        vec!["--max-guests-per-host", "256"],
        vec!["--ttl-secs", "0"],
    ] {
        let out = run(&args);
        assert_eq!(code(&out), 2, "{args:?} should be a usage error");
    }
}

#[test]
fn public_bind_requires_explicit_opt_in() {
    let out = run(&["--bind", "0.0.0.0:25580"]);
    assert_eq!(code(&out), 2, "public bind must be refused");
    assert!(
        stderr(&out).contains("--public-bind"),
        "refusal should name the opt-in flag: {}",
        stderr(&out)
    );

    let out = run(&["--bind", "0.0.0.0:0", "--public-bind", "--self-test"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
}

#[test]
fn self_test_exits_zero() {
    let out = run(&["--self-test"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(stdout(&out).contains("PASS"), "{}", stdout(&out));
}

#[test]
fn live_relay_enforces_caps_and_forwards_frames() {
    let probe = TcpListener::bind(loopback_addr()).unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let child = Command::new(BIN)
        .args([
            "--bind",
            &format!("127.0.0.1:{port}"),
            "--max-hosts",
            "1",
            "--max-guests-per-host",
            "1",
            "--ttl-secs",
            "30",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _guard = ChildGuard { child };
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);

    // The first host registers; its requested 200 guests are clamped to 1.
    let mut host = wait_for_listener(port);
    let code = register_host(&mut host, 200);
    assert!(code.to_string().starts_with("MEGU-"));

    // The single host slot is occupied: a second HostRegister fails closed.
    let mut second_host = Link::connect(addr).unwrap();
    second_host
        .send(&FriendPacket::HostRegister {
            protocol_version: 767,
            world_name: "Second Host".to_owned(),
            max_guests: 1,
            host_secret: HOST_SECRET,
        })
        .unwrap();
    assert!(second_host.expect_closed(), "second host must be refused");

    // Guest one is admitted on the clamped single slot.
    let mut guest1 = Link::connect(addr).unwrap();
    let stream1 = join_guest(&mut guest1, code, "Alice").unwrap();
    match host.recv().unwrap() {
        FriendPacket::StreamOpen {
            stream_id,
            guest_name,
        } => {
            assert_eq!(stream_id, stream1);
            assert_eq!(guest_name, "Alice");
        }
        other => panic!("expected StreamOpen, got {other:?}"),
    }

    // Guest two hits the clamped per-host cap.
    let mut guest2 = Link::connect(addr).unwrap();
    assert_eq!(
        join_guest(&mut guest2, code, "Bob").unwrap_err(),
        "world is full"
    );

    // Real opaque bytes bridge in both directions on the admitted stream.
    let upstream = b"guest-to-host-cli-test";
    guest1
        .send(&FriendPacket::StreamData {
            stream_id: stream1,
            payload: upstream.to_vec(),
        })
        .unwrap();
    expect_stream_data(&mut host, stream1, upstream);

    let downstream = b"host-to-guest-cli-test";
    host.send(&FriendPacket::StreamData {
        stream_id: stream1,
        payload: downstream.to_vec(),
    })
    .unwrap();
    expect_stream_data(&mut guest1, stream1, downstream);
}
