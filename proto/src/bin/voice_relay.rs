// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `megu-voice-relay`: zero-dependency operator sidecar for the `VCR1`
//! UDP proximity voice relay implemented by `megu_proto::voice_relay`.
//!
//! Three modes:
//!
//! * **serve** (default) — bind `--bind` and drain datagrams forever with
//!   `UdpVoiceRelay::poll_once`. The player table starts empty: until a game
//!   host feeds it player state every `Auth` fails closed, which is the
//!   fail-safe posture, not a bug — the socket still exercises the live
//!   decode/drop path and reports metrics.
//! * `--bench-packets N` — a self-contained offline routing benchmark: N
//!   `route_mic` calls over a synthetic table of 32 authenticated players
//!   spread across 3 dimensions, each call timed with [`Instant`] and
//!   reported as p50/p99/mean/max latency plus packets-per-second. No
//!   socket is ever opened.
//! * `--self-test` — a bounded end-to-end smoke check: an ephemeral
//!   `127.0.0.1:0` `UdpVoiceRelay`, two loopback client sockets, secret
//!   auth, proximity delivery at 10 m, whisper gating and normal audibility
//!   at 25 m under the library's default distances.
//!
//! Exit codes: `0` success, `1` runtime failure (bind error, self-test
//! failure), `2` bad command line — unknown option, missing or invalid
//! value, or a non-loopback `--bind` refused without `--allow-public-bind`.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use megu_proto::voice_relay::{
    decode_packet, encode_packet, UdpVoiceRelay, VoicePacket, VoicePlayerState, VoiceRelayConfig,
    VoiceRelayState,
};

const HELP: &str = "\
megu-voice-relay — zero-dependency `VCR1` UDP proximity voice relay

USAGE:
    megu-voice-relay [OPTIONS]

OPTIONS:
    --bind <ip:port>           UDP socket to serve on (default 127.0.0.1:24454).
                               Only loopback binds are permitted unless
                               --allow-public-bind is passed.
    --allow-public-bind        Opt in to binding a non-loopback address. The
                               default posture matches megu: LAN exposure
                               must be a deliberate operator choice.
    --max-distance <f64>       Earshot for normal speech, blocks (default 48.0).
    --whisper-distance <f64>   Earshot for whispered frames, blocks (default 16.0).
    --max-pps <u32>            MicFrames one sender may forward per second
                               (default 64).
    --bench-packets <N>        Run an offline routing benchmark: N MicFrames
                               over a synthetic 32-player table spread across
                               3 dimensions, reporting p50/p99 routing latency
                               and packets/second. No socket is opened.
    --json                     Emit the benchmark report as JSON.
    --self-test                Run a bounded loopback check on an ephemeral
                               bind: secret auth, 10 m proximity delivery,
                               16 m whisper gating, then exit.
    -h, --help                 Show this help.

EXIT CODES:
    0  success
    1  runtime failure (socket bind error, failed --self-test)
    2  bad command line, or a non-loopback --bind refused without
       --allow-public-bind

NOTE:
    The serve loop starts with an empty player table: until a game host
    feeds it player state, every Auth fails closed. That is the fail-safe
    posture, not a bug.
";

const DEFAULT_BIND: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 24454);

/// Players simulated by `--bench-packets`.
const BENCH_PLAYERS: usize = 32;
/// Dimensions the bench table is spread over.
const BENCH_DIMS: [&str; 3] = [
    "minecraft:overworld",
    "minecraft:the_nether",
    "minecraft:the_end",
];
/// Fake-clock step between benched frames. With 32 senders rotating, each
/// sender's frames land ~32 ms apart, so the default 64/s rate window keeps
/// ~32 entries — comfortably under the cap while still exercising the
/// window-drain path. `route_mic` only ever sees monotonic `Instant`s.
const BENCH_FRAME_STEP_MS: u64 = 1;

/// Everything parsed off the command line, defaults applied.
struct Cli {
    bind: SocketAddr,
    allow_public_bind: bool,
    config: VoiceRelayConfig,
    bench_packets: Option<u64>,
    json: bool,
    self_test: bool,
}

enum Parsed {
    Help,
    Run(Cli),
}

/// The value for `--name`: either the inline `=value` form or the next argv
/// word, which `i` is advanced past.
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

fn parse_distance(v: &str, name: &str) -> Result<f64, String> {
    match v.parse::<f64>() {
        Ok(d) if d.is_finite() && d > 0.0 => Ok(d),
        _ => Err(format!(
            "invalid {name} `{v}`: expected a positive finite number"
        )),
    }
}

fn parse_u32(v: &str, name: &str) -> Result<u32, String> {
    match v.parse::<u32>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!(
            "invalid {name} `{v}`: expected a positive integer"
        )),
    }
}

fn parse_u64(v: &str, name: &str) -> Result<u64, String> {
    match v.parse::<u64>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!(
            "invalid {name} `{v}`: expected a positive integer"
        )),
    }
}

fn parse_args(argv: &[String]) -> Result<Parsed, String> {
    let mut cli = Cli {
        bind: DEFAULT_BIND,
        allow_public_bind: false,
        config: VoiceRelayConfig::default(),
        bench_packets: None,
        json: false,
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
            "--allow-public-bind" => cli.allow_public_bind = true,
            "--max-distance" => {
                let v = value(argv, &mut i, inline, "--max-distance")?;
                cli.config.max_distance = parse_distance(v, "--max-distance")?;
            }
            "--whisper-distance" => {
                let v = value(argv, &mut i, inline, "--whisper-distance")?;
                cli.config.whisper_distance = parse_distance(v, "--whisper-distance")?;
            }
            "--max-pps" => {
                let v = value(argv, &mut i, inline, "--max-pps")?;
                cli.config.max_packets_per_window = parse_u32(v, "--max-pps")?;
            }
            "--bench-packets" => {
                let v = value(argv, &mut i, inline, "--bench-packets")?;
                cli.bench_packets = Some(parse_u64(v, "--bench-packets")?);
            }
            "--json" => cli.json = true,
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
            eprintln!("error: {e}\nrun `megu-voice-relay --help` for usage");
            return ExitCode::from(2);
        }
    };
    // Security posture check up front so it applies uniformly: a public
    // bind is refused no matter which mode the flag combination selects.
    if !cli.bind.ip().is_loopback() && !cli.allow_public_bind {
        eprintln!(
            "error: refusing to bind non-loopback address {} \
             (pass --allow-public-bind to override)",
            cli.bind
        );
        return ExitCode::from(2);
    }
    if cli.self_test {
        match self_test() {
            Ok(()) => println!(
                "self-test: PASS (loopback auth x2, 10m proximity, 25m whisper-gated, 25m audible)"
            ),
            Err(e) => {
                eprintln!("self-test: FAIL — {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    if let Some(frames) = cli.bench_packets {
        let report = run_bench(&cli.config, frames);
        if cli.json {
            println!("{}", report.to_json());
        } else {
            print!("{}", report.to_text());
        }
    }
    if cli.self_test || cli.bench_packets.is_some() {
        return ExitCode::SUCCESS;
    }
    match serve(cli.bind, cli.config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: serve on {}: {e}", cli.bind);
            ExitCode::FAILURE
        }
    }
}

/// The serve loop: one non-blocking drain per tick plus a heartbeat of the
/// relay counters on stderr. Stdout stays reserved for machine-readable
/// output (the `--json` benchmark report).
fn serve(bind: SocketAddr, config: VoiceRelayConfig) -> io::Result<()> {
    let mut relay = UdpVoiceRelay::bind(bind, VoiceRelayState::new(config.clone()))?;
    eprintln!(
        "megu-voice-relay: VCR1 listening on {} \
         (max_distance={}, whisper_distance={}, max_pps={})",
        relay.local_addr()?,
        config.max_distance,
        config.whisper_distance,
        config.max_packets_per_window,
    );
    eprintln!("  player table is empty until a game host feeds it; every Auth fails closed");
    let mut last_report = Instant::now();
    loop {
        relay.poll_once(Instant::now())?;
        if last_report.elapsed() >= Duration::from_secs(10) {
            let m = relay.metrics();
            eprintln!(
                "metrics: rx={} tx={} auth_ok={} auth_rejected={} \
                 dropped_malformed={} dropped_rate_limited={}",
                m.rx_packets,
                m.tx_packets,
                m.auth_ok,
                m.auth_rejected,
                m.dropped_malformed,
                m.dropped_rate_limited,
            );
            last_report = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// What one `--bench-packets N` run measured.
struct BenchReport {
    frames: u64,
    routed: u64,
    rejected: u64,
    deliveries: u64,
    elapsed: Duration,
    p50_ns: u64,
    p99_ns: u64,
    mean_ns: u64,
    max_ns: u64,
}

impl BenchReport {
    fn packets_per_second(&self) -> f64 {
        self.frames as f64 / self.elapsed.as_secs_f64().max(1e-9)
    }

    fn to_text(&self) -> String {
        format!(
            "voice-relay bench: {} frames over {} players in {} dimensions\n\
            \x20 routed:      {} frames ({} rejected)\n\
            \x20 deliveries:  {} ({:.2} per routed frame)\n\
            \x20 elapsed:     {:.3} ms\n\
            \x20 throughput:  {:.1} packets/s\n\
            \x20 latency:     p50 {:.2} us, p99 {:.2} us, mean {:.2} us, max {:.2} us\n",
            self.frames,
            BENCH_PLAYERS,
            BENCH_DIMS.len(),
            self.routed,
            self.rejected,
            self.deliveries,
            self.deliveries as f64 / self.routed.max(1) as f64,
            self.elapsed.as_secs_f64() * 1e3,
            self.packets_per_second(),
            self.p50_ns as f64 / 1e3,
            self.p99_ns as f64 / 1e3,
            self.mean_ns as f64 / 1e3,
            self.max_ns as f64 / 1e3,
        )
    }

    fn to_json(&self) -> String {
        format!(
            "{{\"frames\":{},\"players\":{},\"dimensions\":{},\"routed\":{},\
             \"rejected\":{},\"deliveries\":{},\"elapsed_seconds\":{:.6},\
             \"packets_per_second\":{:.2},\"latency_ns\":{{\"p50\":{},\"p99\":{},\
             \"mean\":{},\"max\":{}}}}}",
            self.frames,
            BENCH_PLAYERS,
            BENCH_DIMS.len(),
            self.routed,
            self.rejected,
            self.deliveries,
            self.elapsed.as_secs_f64(),
            self.packets_per_second(),
            self.p50_ns,
            self.p99_ns,
            self.mean_ns,
            self.max_ns,
        )
    }
}

/// Nearest-rank percentile over a sorted sample set.
fn percentile(sorted: &[u64], p: u64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let n = sorted.len() as u64;
    let rank = (p * n).div_ceil(100);
    sorted[(rank.max(1) - 1) as usize]
}

/// Build the synthetic bench table: 32 authenticated players, a third in
/// each dimension, scattered over a +/-30 block square so some pairs fall
/// inside the earshot radii and some outside.
fn bench_state(config: &VoiceRelayConfig) -> (VoiceRelayState, Vec<[u8; 16]>) {
    let mut state = VoiceRelayState::new(config.clone());
    let mut uuids = Vec::with_capacity(BENCH_PLAYERS);
    for i in 0..BENCH_PLAYERS {
        let uuid = [i as u8; 16];
        let mut p = VoicePlayerState::new(uuid, [0xC3; 16]);
        p.dimension = BENCH_DIMS[i % BENCH_DIMS.len()].to_owned();
        p.pos = [
            ((i * 7) % 60) as f64 - 30.0,
            64.0,
            ((i * 11) % 60) as f64 - 30.0,
        ];
        p.peer_addr = Some(SocketAddr::from(([127, 0, 0, 1], 10_000 + i as u16)));
        p.authenticated = true;
        state.upsert_player(p);
        uuids.push(uuid);
    }
    (state, uuids)
}

/// Time `frames` `route_mic` calls over the synthetic table, rotating the
/// sender so every player speaks in turn and whispering every 8th frame so
/// both distance limits are exercised. The fake clock handed to the router
/// steps [`BENCH_FRAME_STEP_MS`] per frame; the wall clock around each call
/// is what the latency percentiles measure.
fn run_bench(config: &VoiceRelayConfig, frames: u64) -> BenchReport {
    let (mut state, uuids) = bench_state(config);
    let base = Instant::now();
    let mut latencies = Vec::with_capacity(frames as usize);
    let mut routed = 0u64;
    let mut rejected = 0u64;
    let mut deliveries = 0u64;
    let started = Instant::now();
    for i in 0..frames {
        let sender = uuids[i as usize % BENCH_PLAYERS];
        let route_now = base + Duration::from_millis(i * BENCH_FRAME_STEP_MS);
        let t0 = Instant::now();
        let out = state.route_mic(&sender, i % 8 == 0, route_now);
        let ns = t0.elapsed().as_nanos();
        latencies.push(u64::try_from(ns).unwrap_or(u64::MAX));
        match out {
            Ok(ds) => {
                routed += 1;
                deliveries += ds.len() as u64;
            }
            Err(_) => rejected += 1,
        }
    }
    let elapsed = started.elapsed();
    latencies.sort_unstable();
    let sum: u128 = latencies.iter().map(|&v| u128::from(v)).sum();
    BenchReport {
        frames,
        routed,
        rejected,
        deliveries,
        elapsed,
        p50_ns: percentile(&latencies, 50),
        p99_ns: percentile(&latencies, 99),
        mean_ns: (sum / latencies.len().max(1) as u128) as u64,
        max_ns: *latencies.last().unwrap_or(&0),
    }
}

/// One loopback "voice client" for `--self-test`: a bound UDP socket plus
/// the identity the relay was told to expect.
struct LoopbackClient {
    sock: UdpSocket,
    uuid: [u8; 16],
    secret: [u8; 16],
}

impl LoopbackClient {
    fn bind(uuid_byte: u8, secret_byte: u8) -> io::Result<Self> {
        Ok(Self {
            sock: UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))?,
            uuid: [uuid_byte; 16],
            secret: [secret_byte; 16],
        })
    }

    fn send(&self, relay: SocketAddr, packet: &VoicePacket) -> io::Result<()> {
        let bytes = encode_packet(packet).expect("a well-formed VCR1 packet");
        self.sock.send_to(&bytes, relay).map(|_| ())
    }

    /// Receive and decode one datagram, or `None` after `ms`.
    fn recv(&self, ms: u64) -> Option<VoicePacket> {
        if self
            .sock
            .set_read_timeout(Some(Duration::from_millis(ms)))
            .is_err()
        {
            return None;
        }
        let mut buf = [0u8; 2048];
        match self.sock.recv_from(&mut buf) {
            Ok((n, _)) => decode_packet(&buf[..n]).ok(),
            Err(_) => None,
        }
    }
}

/// Drain the relay once it has at least one datagram; loopback delivery is
/// asynchronous by a few microseconds, so poll a bounded number of times.
fn pump(relay: &mut UdpVoiceRelay) -> io::Result<()> {
    for _ in 0..200 {
        if relay.poll_once(Instant::now())? > 0 {
            relay.poll_once(Instant::now())?;
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

/// The `--self-test` smoke check: real loopback UDP through the whole
/// `VCR1` path under the library's default distances.
fn self_test() -> Result<(), String> {
    let ioe = |e: io::Error| format!("io: {e}");
    let mut relay = UdpVoiceRelay::bind(
        SocketAddr::from(([127, 0, 0, 1], 0)),
        VoiceRelayState::new(VoiceRelayConfig::default()),
    )
    .map_err(ioe)?;
    let relay_addr = relay.local_addr().map_err(ioe)?;
    let alice = LoopbackClient::bind(0xA1, 0x11).map_err(ioe)?;
    let bob = LoopbackClient::bind(0xB0, 0x22).map_err(ioe)?;

    for (c, pos) in [(&alice, [0.0, 64.0, 0.0]), (&bob, [10.0, 64.0, 0.0])] {
        let mut p = VoicePlayerState::new(c.uuid, c.secret);
        p.dimension = "minecraft:overworld".to_owned();
        p.pos = pos;
        relay.state_mut().upsert_player(p);
    }
    for c in [&alice, &bob] {
        c.send(
            relay_addr,
            &VoicePacket::Auth {
                player_uuid: c.uuid,
                secret: c.secret,
            },
        )
        .map_err(ioe)?;
        pump(&mut relay).map_err(ioe)?;
        match c.recv(2000) {
            Some(VoicePacket::AuthAck { ok: true }) => {}
            other => return Err(format!("auth for {:02x?} did not ack ok: {other:?}", c.uuid)),
        }
    }

    // Normal speech at 10 m: the verbatim payload arrives with the exact
    // squared distance on the proximity channel.
    let opus = b"self-test-opus-frame";
    alice
        .send(
            relay_addr,
            &VoicePacket::MicFrame {
                player_uuid: alice.uuid,
                seq: 1,
                whispering: false,
                opus_payload: opus.to_vec(),
            },
        )
        .map_err(ioe)?;
    pump(&mut relay).map_err(ioe)?;
    match bob.recv(2000) {
        Some(VoicePacket::PlayerSound {
            sender_uuid,
            seq: 1,
            whispering: false,
            distance_sq,
            group: false,
            opus_payload,
        }) if sender_uuid == alice.uuid && distance_sq == 100.0 && opus_payload == opus => {}
        other => {
            return Err(format!(
                "expected PlayerSound seq=1 distance_sq=100 for bob, got {other:?}"
            ))
        }
    }

    // Move bob to 25 m: outside the 16 m whisper radius a whispered frame
    // must produce silence, while normal speech (25 m < 48 m) still lands.
    relay
        .state_mut()
        .player_mut(&bob.uuid)
        .ok_or("bob missing from the player table")?
        .pos = [25.0, 64.0, 0.0];
    let whisper = |seq: u64, whispering: bool| VoicePacket::MicFrame {
        player_uuid: alice.uuid,
        seq,
        whispering,
        opus_payload: opus.to_vec(),
    };
    alice.send(relay_addr, &whisper(2, true)).map_err(ioe)?;
    pump(&mut relay).map_err(ioe)?;
    if let Some(p) = bob.recv(150) {
        return Err(format!("whispered frame leaked past the 16m radius: {p:?}"));
    }
    alice.send(relay_addr, &whisper(3, false)).map_err(ioe)?;
    pump(&mut relay).map_err(ioe)?;
    match bob.recv(2000) {
        Some(VoicePacket::PlayerSound {
            sender_uuid,
            seq: 3,
            distance_sq,
            group: false,
            opus_payload,
            ..
        }) if sender_uuid == alice.uuid && distance_sq == 625.0 && opus_payload == opus => Ok(()),
        other => Err(format!(
            "expected PlayerSound seq=3 distance_sq=625 for bob, got {other:?}"
        )),
    }
}
