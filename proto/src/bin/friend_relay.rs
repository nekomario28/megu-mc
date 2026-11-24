// SPDX-License-Identifier: LGPL-3.0-or-later
//! `megu-friend-relay`: standalone TCP operator for the `FRL2` friend
//! invite relay implemented by `megu_proto::friend_relay`.
//!
//! The public listener is a small fail-closed edge in front of an internal
//! loopback [`TcpFriendRelay`]. It parses only the tiny `FRL2` control frames
//! needed for admission (`HostRegister`, `HostRegistered`, `HostRevoked`) and
//! forwards every other frame verbatim, so guest and host Minecraft bytes keep
//! the library relay's tested framing path. The edge enforces the operator
//! limits the router itself does not own: a bound on live host links and a
//! per-host guest cap applied by clamping `HostRegister.max_guests`.
//!
//! Exit codes: `0` success, `1` runtime failure, `2` bad command line —
//! unknown option, missing/invalid value, or a non-loopback `--bind` refused
//! without `--public-bind`.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use megu_proto::frl_auth;
use megu_proto::friend_relay::{
    decode_frame, decode_packet, encode_frame, FriendPacket, FriendRelayConfig, FriendRelayRouter,
    InviteCode, TcpFriendRelay, FRIEND_MAGIC, MAX_FRL2_FRAME_BYTES,
};

const HELP: &str = "\
megu-friend-relay — standalone FRL2 friend invite relay

USAGE:
    megu-friend-relay [OPTIONS]

OPTIONS:
    --bind <ip:port>             TCP address to serve on (default 127.0.0.1:25580).
                                 Only loopback binds are permitted unless
                                 --public-bind is passed.
    --public-bind                Opt in to binding a non-loopback address. The
                                 default posture is fail-closed: LAN/WAN exposure
                                 must be a deliberate operator choice.
    --max-hosts <n>              Maximum live host registrations admitted at once
                                 (default 64).
    --max-guests-per-host <n>    Clamp every HostRegister's max_guests to this
                                 value (default 8, maximum 255).
    --ttl-secs <n>               Invite-code lifetime in seconds (default 28800,
                                 eight hours).
    --self-test                  Run a bounded loopback host/guest/frame check,
                                 then exit.
    -h, --help                   Show this help.

EXIT CODES:
    0  success
    1  runtime failure (bind error, failed --self-test)
    2  bad command line, or a non-loopback --bind refused without
       --public-bind
";

const DEFAULT_BIND: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 25580);
const DEFAULT_MAX_HOSTS: u64 = 64;
const DEFAULT_MAX_GUESTS_PER_HOST: u8 = 8;
const DEFAULT_TTL_SECS: u64 = 8 * 60 * 60;
const INTERNAL_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REPORT_INTERVAL: Duration = Duration::from_secs(10);

// FRL2 tag bytes for the small control packets the admission edge must see.
// Payload-bearing StreamData frames are forwarded without being decoded.
const TAG_HOST_REGISTER: u8 = 0x01;
const TAG_HOST_REGISTERED: u8 = 0x02;
const TAG_HOST_REVOKED: u8 = 0x04;

#[derive(Clone, Copy)]
struct RelayLimits {
    max_hosts: u64,
    max_guests_per_host: u8,
}

#[derive(Default)]
struct HostAccounting {
    count: usize,
}

#[derive(Default)]
struct ConnGate {
    pending: usize,
    active: usize,
    closed: bool,
}

type SharedHosts = Arc<Mutex<HostAccounting>>;
type SharedGate = Arc<Mutex<ConnGate>>;

struct Cli {
    bind: SocketAddr,
    public_bind: bool,
    limits: RelayLimits,
    ttl_secs: u64,
    self_test: bool,
}

enum Parsed {
    Help,
    Run(Cli),
}

fn value<'a>(
    argv: &'a [String],
    i: &mut usize,
    inline: Option<&'a str>,
    name: &str,
) -> Result<&'a str, String> {
    if let Some(v) = inline {
        return Ok(v);
    }
    *i += 1;
    argv.get(*i)
        .map(String::as_str)
        .ok_or_else(|| format!("{name} requires a value"))
}

fn parse_u64(v: &str, name: &str) -> Result<u64, String> {
    match v.parse::<u64>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!("invalid {name} `{v}`: expected a positive integer")),
    }
}

fn parse_u8(v: &str, name: &str) -> Result<u8, String> {
    match v.parse::<u8>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!(
            "invalid {name} `{v}`: expected an integer in 1..=255"
        )),
    }
}

fn parse_args(argv: &[String]) -> Result<Parsed, String> {
    let mut cli = Cli {
        bind: DEFAULT_BIND,
        public_bind: false,
        limits: RelayLimits {
            max_hosts: DEFAULT_MAX_HOSTS,
            max_guests_per_host: DEFAULT_MAX_GUESTS_PER_HOST,
        },
        ttl_secs: DEFAULT_TTL_SECS,
        self_test: false,
    };
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v)),
            _ => (arg.as_str(), None),
        };
        match name {
            "-h" | "--help" => return Ok(Parsed::Help),
            "--bind" => {
                let v = value(argv, &mut i, inline, "--bind")?;
                cli.bind = v
                    .parse()
                    .map_err(|_| format!("invalid --bind `{v}`: expected <ip:port>"))?;
            }
            "--public-bind" => cli.public_bind = true,
            "--max-hosts" => {
                let v = value(argv, &mut i, inline, "--max-hosts")?;
                cli.limits.max_hosts = parse_u64(v, "--max-hosts")?;
            }
            "--max-guests-per-host" => {
                let v = value(argv, &mut i, inline, "--max-guests-per-host")?;
                cli.limits.max_guests_per_host = parse_u8(v, "--max-guests-per-host")?;
            }
            "--ttl-secs" => {
                let v = value(argv, &mut i, inline, "--ttl-secs")?;
                cli.ttl_secs = parse_u64(v, "--ttl-secs")?;
            }
            "--self-test" => cli.self_test = true,
            other => return Err(format!("unknown option `{other}`")),
        }
        i += 1;
    }
    Ok(Parsed::Run(cli))
}

fn main() -> ExitCode {
    let argv: Vec<String> = match std::env::args_os()
        .skip(1)
        .map(|a| a.into_string())
        .collect()
    {
        Ok(v) => v,
        Err(_) => {
            eprintln!("error: non-UTF-8 argument");
            return ExitCode::from(2);
        }
    };
    let cli = match parse_args(&argv) {
        Ok(Parsed::Help) => {
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Run(c)) => c,
        Err(e) => {
            eprintln!("error: {e}\nrun `megu-friend-relay --help` for usage");
            return ExitCode::from(2);
        }
    };
    if !cli.bind.ip().is_loopback() && !cli.public_bind {
        eprintln!(
            "error: refusing to bind non-loopback address {} \
             (pass --public-bind to override)",
            cli.bind
        );
        return ExitCode::from(2);
    }
    if cli.self_test {
        return match self_test() {
            Ok(()) => {
                println!("self-test: PASS (public edge, host cap, guest cap, bidirectional FRL2)");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("self-test: FAIL — {e}");
                ExitCode::FAILURE
            }
        };
    }
    match serve(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: serve on {}: {e}", cli.bind);
            ExitCode::FAILURE
        }
    }
}

fn serve(cli: &Cli) -> io::Result<()> {
    let listener = TcpListener::bind(cli.bind)?;
    listener.set_nonblocking(true)?;
    let public_addr = listener.local_addr()?;

    let router = FriendRelayRouter::new(FriendRelayConfig {
        invite_ttl: Duration::from_secs(cli.ttl_secs),
    });
    let internal_bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let mut internal = TcpFriendRelay::bind_with_router(internal_bind, router)?;
    let internal_addr = internal.local_addr()?;
    let (poll_err_tx, poll_err_rx) = mpsc::channel::<String>();
    thread::spawn(move || loop {
        match internal.poll_once(Instant::now()) {
            Ok(_) => thread::sleep(Duration::from_millis(1)),
            Err(e) => {
                let _ = poll_err_tx.send(format!("FRL2 poll failed: {e}"));
                return;
            }
        }
    });

    let hosts: SharedHosts = Arc::new(Mutex::new(HostAccounting::default()));
    let limits = cli.limits;
    eprintln!(
        "megu-friend-relay: FRL2 listening on {} \
         (max_hosts={}, max_guests_per_host={}, invite_ttl_secs={})",
        public_addr, cli.limits.max_hosts, cli.limits.max_guests_per_host, cli.ttl_secs,
    );
    eprintln!("  internal FRL2 relay is loopback-only at {internal_addr}");

    let mut last_report = Instant::now();
    loop {
        if let Ok(err) = poll_err_rx.try_recv() {
            return Err(io::Error::other(err));
        }
        match listener.accept() {
            Ok((client, peer)) => {
                // On macOS/BSD, accepted sockets inherit O_NONBLOCK from a
                // non-blocking listener; restore blocking mode for the
                // per-connection pump threads.
                if let Err(e) = client.set_nonblocking(false) {
                    eprintln!("megu-friend-relay: client {peer}: set_nonblocking(false): {e}");
                    continue;
                }
                let _ = client.set_nodelay(true);
                let hosts = Arc::clone(&hosts);
                thread::spawn(move || {
                    if let Err(e) = bridge_client(client, internal_addr, hosts, limits) {
                        eprintln!("megu-friend-relay: client {peer}: {e}");
                    }
                });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // BSD/macOS surfaces a pending connection that was reset before
            // accept() dequeued it as ECONNABORTED/ECONNRESET: a per-connection
            // event that must not kill the listener itself.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset
                ) => {}
            Err(e) => return Err(e),
        }
        if last_report.elapsed() >= REPORT_INTERVAL {
            let host_links = hosts.lock().map(|h| h.count).unwrap_or(0);
            eprintln!("metrics: host_links={host_links}");
            last_report = Instant::now();
        }
        thread::sleep(Duration::from_millis(1));
    }
}

fn bridge_client(
    client: TcpStream,
    internal_addr: SocketAddr,
    hosts: SharedHosts,
    limits: RelayLimits,
) -> io::Result<()> {
    let internal = TcpStream::connect_timeout(&internal_addr, INTERNAL_CONNECT_TIMEOUT)?;
    internal.set_nodelay(true)?;
    let client_out = client.try_clone()?;
    let internal_out = internal.try_clone()?;
    let gate: SharedGate = Arc::new(Mutex::new(ConnGate::default()));

    let client_gate = Arc::clone(&gate);
    let client_hosts = Arc::clone(&hosts);
    thread::spawn(move || {
        let _ = pump_client_to_relay(client, internal_out, limits, client_hosts, client_gate);
    });
    let relay_gate = Arc::clone(&gate);
    let relay_hosts = Arc::clone(&hosts);
    thread::spawn(move || {
        let _ = pump_relay_to_client(internal, client_out, relay_hosts, relay_gate);
    });
    Ok(())
}

fn host_register_admit(hosts: &SharedHosts, gate: &SharedGate, max_hosts: u64) -> io::Result<()> {
    let mut accounting = hosts
        .lock()
        .map_err(|_| io::Error::other("host accounting poisoned"))?;
    let mut conn = gate
        .lock()
        .map_err(|_| io::Error::other("connection gate poisoned"))?;
    if accounting.count as u64 >= max_hosts {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "friend relay host limit reached",
        ));
    }
    accounting.count += 1;
    conn.pending += 1;
    Ok(())
}

fn note_host_registered(gate: &SharedGate) {
    if let Ok(mut conn) = gate.lock() {
        if conn.pending > 0 {
            conn.pending -= 1;
            conn.active += 1;
        }
    }
}

fn note_host_revoked(hosts: &SharedHosts, gate: &SharedGate) {
    let Ok(mut accounting) = hosts.lock() else {
        return;
    };
    let Ok(mut conn) = gate.lock() else {
        return;
    };
    if conn.active > 0 {
        conn.active -= 1;
        accounting.count = accounting.count.saturating_sub(1);
    }
}

fn account_close(hosts: &SharedHosts, gate: &SharedGate) {
    let (Ok(mut accounting), Ok(mut conn)) = (hosts.lock(), gate.lock()) else {
        return;
    };
    if conn.closed {
        return;
    }
    conn.closed = true;
    let released = conn.pending + conn.active;
    conn.pending = 0;
    conn.active = 0;
    accounting.count = accounting.count.saturating_sub(released);
}

fn frame_body(frame: &[u8]) -> Option<&[u8]> {
    if frame.len() > 4 + FRIEND_MAGIC.len() {
        let len = u32::from_be_bytes(frame[..4].try_into().ok()?) as usize;
        if (5..=MAX_FRL2_FRAME_BYTES).contains(&len) && frame.len() == 4 + len {
            return Some(&frame[4..]);
        }
    }
    None
}

fn body_tag(body: &[u8], tag: u8) -> bool {
    body.len() > FRIEND_MAGIC.len()
        && body[..FRIEND_MAGIC.len()] == FRIEND_MAGIC
        && body[FRIEND_MAGIC.len()] == tag
}

fn frame_tag(frame: &[u8], tag: u8) -> bool {
    frame_body(frame).is_some_and(|body| body_tag(body, tag))
}

fn next_frame_len(inbuf: &[u8]) -> io::Result<Option<usize>> {
    if inbuf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes(inbuf[..4].try_into().unwrap()) as usize;
    if !(5..=MAX_FRL2_FRAME_BYTES).contains(&len) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("FRL2 frame length {len} outside 5..={MAX_FRL2_FRAME_BYTES}"),
        ));
    }
    if inbuf.len() < 4 + len {
        return Ok(None);
    }
    Ok(Some(4 + len))
}

fn pump_client_to_relay(
    client: TcpStream,
    relay: TcpStream,
    limits: RelayLimits,
    hosts: SharedHosts,
    gate: SharedGate,
) -> io::Result<()> {
    let mut client = client;
    let mut relay = relay;
    let result = pump_client_frames(&mut client, &mut relay, limits, &hosts, &gate);
    account_close(&hosts, &gate);
    let _ = relay.shutdown(Shutdown::Both);
    let _ = client.shutdown(Shutdown::Both);
    result
}

fn pump_client_frames(
    client: &mut TcpStream,
    relay: &mut TcpStream,
    limits: RelayLimits,
    hosts: &SharedHosts,
    gate: &SharedGate,
) -> io::Result<()> {
    let mut inbuf = Vec::with_capacity(8192);
    let mut tmp = [0u8; 8192];
    loop {
        let n = match client.read(&mut tmp) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        inbuf.extend_from_slice(&tmp[..n]);
        while let Some(total) = next_frame_len(&inbuf)? {
            let frame: Vec<u8> = inbuf.drain(..total).collect();
            let body = &frame[4..];
            if body_tag(body, TAG_HOST_REGISTER) {
                match decode_packet(body) {
                    Ok(FriendPacket::HostRegister {
                        protocol_version,
                        world_name,
                        max_guests,
                        session_key,
                    }) => {
                        host_register_admit(hosts, gate, limits.max_hosts)?;
                        let packet = FriendPacket::HostRegister {
                            protocol_version,
                            world_name,
                            max_guests: max_guests.min(limits.max_guests_per_host),
                            session_key,
                        };
                        let rewritten = encode_frame(&packet).map_err(io::Error::other)?;
                        relay.write_all(&rewritten)?;
                    }
                    _ => relay.write_all(&frame)?,
                }
            } else {
                relay.write_all(&frame)?;
            }
        }
    }
}

fn pump_relay_to_client(
    relay: TcpStream,
    client: TcpStream,
    hosts: SharedHosts,
    gate: SharedGate,
) -> io::Result<()> {
    let mut relay = relay;
    let mut client = client;
    let result = pump_relay_frames(&mut relay, &mut client, &hosts, &gate);
    account_close(&hosts, &gate);
    let _ = client.shutdown(Shutdown::Both);
    let _ = relay.shutdown(Shutdown::Both);
    result
}

fn pump_relay_frames(
    relay: &mut TcpStream,
    client: &mut TcpStream,
    hosts: &SharedHosts,
    gate: &SharedGate,
) -> io::Result<()> {
    let mut inbuf = Vec::with_capacity(8192);
    let mut tmp = [0u8; 8192];
    loop {
        let n = match relay.read(&mut tmp) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        inbuf.extend_from_slice(&tmp[..n]);
        while let Some(total) = next_frame_len(&inbuf)? {
            let frame: Vec<u8> = inbuf.drain(..total).collect();
            if frame_tag(&frame, TAG_HOST_REGISTERED) {
                note_host_registered(gate);
            } else if frame_tag(&frame, TAG_HOST_REVOKED) {
                if let Ok(FriendPacket::HostRevoked { ok: true }) = decode_frame(&frame) {
                    note_host_revoked(hosts, gate);
                }
            }
            client.write_all(&frame)?;
        }
    }
}

// ---- --self-test: a real spawned edge on an ephemeral loopback port ---------

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
    /// Per-connection `RelayHello` nonce the link's `FRL2` proofs bind to.
    nonce: [u8; 16],
}

impl Link {
    fn connect(addr: SocketAddr) -> io::Result<Self> {
        let sock = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
        sock.set_nodelay(true)?;
        sock.set_read_timeout(Some(Duration::from_secs(3)))?;
        sock.set_write_timeout(Some(Duration::from_secs(3)))?;
        let mut link = Self {
            sock,
            nonce: [0; 16],
        };
        match link.recv()? {
            FriendPacket::RelayHello { nonce } => link.nonce = nonce,
            other => {
                return Err(io::Error::other(format!(
                    "expected RelayHello, got {other:?}"
                )))
            }
        }
        Ok(link)
    }

    /// `FRL2` join: the invite code never crosses the wire — the link proves
    /// possession with `key_id`/`tag` bound to its `RelayHello` nonce.
    fn join(&mut self, code: InviteCode, guest_name: &str) -> io::Result<()> {
        let key_id = frl_auth::join_key_id(&code.as_bytes(), &self.nonce);
        let tag = frl_auth::join_tag(&code.as_bytes(), &key_id, guest_name);
        self.send(&FriendPacket::GuestJoin {
            key_id,
            guest_name: guest_name.to_owned(),
            tag,
        })
    }

    fn send(&mut self, packet: &FriendPacket) -> io::Result<()> {
        let frame = encode_frame(packet).map_err(io::Error::other)?;
        self.sock.write_all(&frame)
    }

    fn recv(&mut self) -> io::Result<FriendPacket> {
        let mut len = [0u8; 4];
        self.sock.read_exact(&mut len)?;
        let n = u32::from_be_bytes(len) as usize;
        if !(5..=MAX_FRL2_FRAME_BYTES).contains(&n) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("self-test frame length {n} outside bounds"),
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

fn ephemeral_port() -> io::Result<u16> {
    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))?;
    Ok(listener.local_addr()?.port())
}

fn spawn_edge(port: u16) -> Result<ChildGuard, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let child = Command::new(exe)
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
        .map_err(|e| format!("spawn self: {e}"))?;
    Ok(ChildGuard { child })
}

fn wait_for_edge(port: u16) -> Option<Link> {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    for _ in 0..120 {
        if let Ok(link) = Link::connect(addr) {
            return Some(link);
        }
        thread::sleep(Duration::from_millis(25));
    }
    None
}

fn self_test() -> Result<(), String> {
    let ioe = |e: io::Error| format!("io: {e}");
    let mut edge = None;
    let mut host = None;
    for _ in 0..3 {
        let port = ephemeral_port().map_err(ioe)?;
        let guard = spawn_edge(port)?;
        if let Some(link) = wait_for_edge(port) {
            edge = Some(guard);
            host = Some(link);
            break;
        }
    }
    let _edge = edge.ok_or("self-test relay did not start")?;
    let mut host = host.ok_or("self-test relay did not accept a connection")?;

    let relay_port = host.sock.peer_addr().map_err(ioe)?.port();
    let relay_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), relay_port);
    let secret = [0x5E; 16];
    host.send(&FriendPacket::HostRegister {
        protocol_version: 767,
        world_name: "Self Test World".to_owned(),
        // Deliberately above the edge cap: the relay must clamp it to 1.
        max_guests: 200,
        session_key: secret,
    })
    .map_err(ioe)?;
    let code = match host.recv().map_err(ioe)? {
        FriendPacket::HostRegistered { invite_code } => invite_code,
        other => return Err(format!("expected HostRegistered, got {other:?}")),
    };

    // A second host is refused at the edge while the first holds the only slot.
    let mut second_host = Link::connect(relay_addr).map_err(ioe)?;
    second_host
        .send(&FriendPacket::HostRegister {
            protocol_version: 767,
            world_name: "Over capacity".to_owned(),
            max_guests: 1,
            session_key: secret,
        })
        .map_err(ioe)?;
    if !second_host.expect_closed() {
        return Err("second HostRegister was not refused by --max-hosts 1".to_owned());
    }

    let mut guest = Link::connect(relay_addr).map_err(ioe)?;
    guest.join(code, "SelfTest").map_err(ioe)?;
    let stream_id = match guest.recv().map_err(ioe)? {
        FriendPacket::GuestAccepted { stream_id } => stream_id,
        other => return Err(format!("expected GuestAccepted, got {other:?}")),
    };
    match host.recv().map_err(ioe)? {
        FriendPacket::StreamOpen {
            stream_id: opened,
            guest_name,
        } if opened == stream_id && guest_name == "SelfTest" => {}
        other => return Err(format!("expected StreamOpen, got {other:?}")),
    }

    let upstream = b"guest-to-host-self-test";
    guest
        .send(&FriendPacket::StreamData {
            stream_id,
            payload: upstream.to_vec(),
        })
        .map_err(ioe)?;
    match host.recv().map_err(ioe)? {
        FriendPacket::StreamData {
            stream_id: got,
            payload,
        } if got == stream_id && payload == upstream => {}
        other => return Err(format!("expected upstream StreamData, got {other:?}")),
    }

    let downstream = b"host-to-guest-self-test";
    host.send(&FriendPacket::StreamData {
        stream_id,
        payload: downstream.to_vec(),
    })
    .map_err(ioe)?;
    match guest.recv().map_err(ioe)? {
        FriendPacket::StreamData {
            stream_id: got,
            payload,
        } if got == stream_id && payload == downstream => {}
        other => return Err(format!("expected downstream StreamData, got {other:?}")),
    }

    // The clamped cap is live: a second guest must be refused.
    let mut late = Link::connect(relay_addr).map_err(ioe)?;
    late.join(code, "Late").map_err(ioe)?;
    match late.recv().map_err(ioe)? {
        FriendPacket::GuestRejected { reason } if reason == "world is full" => {}
        other => return Err(format!("expected world-is-full rejection, got {other:?}")),
    }
    Ok(())
}
