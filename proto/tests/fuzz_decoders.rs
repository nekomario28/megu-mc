// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane P6b (#222): deterministic adversarial fuzz harness for every
//! untrusted wire decoder in `mc-spine-proto`.
//!
//! Every input a remote peer can put on the wire — packet frames in all five
//! phases on both builtin tables (767/1.21.1 and 777/26.3), session-driving
//! frames, `VCR1` voice datagrams, `FRL1` friend-relay frames, invite codes
//! and DNS SRV replies — is hammered with mutated, truncated, bit-flipped,
//! length-bombed and purely random inputs. The contract under test: **zero
//! panics and bounded memory** on every byte string, valid or hostile.
//!
//! Determinism: all inputs derive from a fixed-seed `splitmix64` PRNG — no
//! `SystemTime`, no OS entropy, no external crates. `MC_SPINE_FUZZ_ITERS`
//! scales the per-target iteration count (default `4_000`, so `cargo test`
//! stays under seconds; operators crank `MC_SPINE_FUZZ_ITERS=100000` for a
//! deep soak). A decoder panic is caught, the offending input is hex-dumped
//! for reproduction, and the panic is re-thrown so the test still fails.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use mc_spine_proto::friend_relay::{self, FriendPacket, InviteCode};
use mc_spine_proto::session::{text_component, ServerSession, ServerSessionConfig};
use mc_spine_proto::voice_relay::{self, VoicePacket};
use mc_spine_proto::wire::{
    read_frame, read_frame_bounded, write_frame, WireReader, WireWriter, MAX_STRING_CHARS,
};
use mc_spine_proto::{
    BitSet, BlockPos, C2SPacket, CodecV767, EquipmentSlot, ItemStack, LoginProperty, MenuKind,
    PacketPhase, ProtocolCodec, ProtocolTable, ProtocolVersion, RawComponent, RegistryDataEntry,
    S2CPacket, TableDrivenCodec, TeamInfo, TeamOperation, VersionRouter,
};

/// The DNS SRV reply parser (`resolver::dns`) is deliberately module-private:
/// the crate exposes only `resolve()`. `#[path]` grafts the same source file
/// into this test crate under `resolver_dns`, where `pub(super) fn query_srv`
/// becomes reachable — so fuzzed reply bytes reach `parse_answer`/`read_name`
/// through the real UDP/TCP transport path, over loopback only.
#[path = "../src/resolver/dns.rs"]
mod resolver_dns;

// ---------------------------------------------------------------------------
// Deterministic PRNG (splitmix64) — the same finalizer the codebase uses.
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }

    fn byte(&mut self) -> u8 {
        self.next() as u8
    }

    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
}

/// Per-target iteration count: `MC_SPINE_FUZZ_ITERS` overrides, default 4000.
fn iters() -> usize {
    std::env::var("MC_SPINE_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4_000)
}

// ---------------------------------------------------------------------------
// Panic witness: a decoder panic is caught so the exact input can be dumped
// (hex, one line per 32 bytes) before the panic resumes and fails the test.
// ---------------------------------------------------------------------------

fn hex_dump(bytes: &[u8]) -> String {
    let mut out = String::new();
    for (i, b) in bytes.iter().enumerate() {
        if i % 32 == 0 {
            out.push_str(&format!("\n  {i:06x}:"));
        }
        out.push_str(&format!(" {b:02x}"));
        if out.len() > 1 << 20 {
            out.push_str("\n  ... (dump truncated)");
            break;
        }
    }
    out
}

fn drive(target: &str, input: &[u8], f: impl FnOnce()) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(f)) {
        eprintln!("\n=== FUZZ PANIC in {target} ({} bytes) ===", input.len());
        eprintln!("{}", hex_dump(input));
        resume_unwind(payload);
    }
}

// ---------------------------------------------------------------------------
// Input generators: corpus-guided mutation, random garbage, boundary shapes
// and allocation bombs. Everything stays modest in size — the decoders'
// own length caps are what must fire, not the host's OOM killer.
// ---------------------------------------------------------------------------

/// Attacker-crafted token strings worth splicing anywhere.
const DICTIONARY: &[&[u8]] = &[
    b"\xff\xff\xff\xff\x0f",                 // max 5-byte VarInt: -1
    b"\x80\x80\x80\x80\x80",                 // VarInt that never terminates
    b"\xff\xff\xff\xff\xff",                 // VarInt overflow on byte five
    b"\xff\xff\xff\xff\x7f",                 // VarInt 0x0FFFFFFF
    b"\x7f\xff\xff\xff",                     // VarLong/length prefix fragment
    b"\xff\xff\xff\xff",                     // u32be::MAX / i32be::MIN
    b"\xff\xff\xff\x7f",                     // u32be 0x7FFFFFFF length bomb
    b"\x00\x00\x00\x00",
    b"\x00\x00\x00\x80",                     // i32be::MIN
    b"VCR1",
    b"FRL1",
    b"AUD1",
    b"\x1f\x8b\x08",                         // gzip magic
    b"\x0a\x00\x00\x0a\x00\x00\x0a\x00\x00", // nested anonymous-NBT compounds
    b"\x09\xff\xff\xff\xff",                 // NBT list + huge element count
    b"\x07\x01\x00\x00\x00",                 // NBT byte-array, 16M elements
    b"MEGU-",
    b"{\":\",\"DataVersion\":",
    b"\\u",
];

/// Mutate `seed` with `n` primitive mutations from the standard corpus-guided
/// toolbox: bit flips, byte sets (0/0xFF/random/dictionary splice), inserts,
/// deletes, truncations and tail appends.
fn mutate(rng: &mut Rng, seed: &[u8], max_len: usize) -> Vec<u8> {
    let mut buf = seed.to_vec();
    let edits = 1 + rng.below(8);
    for _ in 0..edits {
        match rng.below(10) {
            // Flip one bit.
            0 if !buf.is_empty() => {
                let i = rng.below(buf.len());
                buf[i] ^= 1 << rng.below(8);
            }
            // Set a byte to an interesting value.
            1 if !buf.is_empty() => {
                let i = rng.below(buf.len());
                let b = rng.byte();
                buf[i] = *rng.pick(&[0x00u8, 0x7f, 0x80, 0xff, b]);
            }
            // Insert a random byte.
            2 if buf.len() < max_len => {
                let i = rng.below(buf.len() + 1);
                buf.insert(i, rng.byte());
            }
            // Delete a byte.
            3 if !buf.is_empty() => {
                let i = rng.below(buf.len());
                buf.remove(i);
            }
            // Delete a span.
            4 if !buf.is_empty() => {
                let i = rng.below(buf.len());
                let n = 1 + rng.below(32.min(buf.len() - i));
                buf.drain(i..i + n);
            }
            // Insert a dictionary token at a random offset.
            5 if buf.len() < max_len => {
                let tok = *rng.pick(DICTIONARY);
                if buf.len() + tok.len() <= max_len {
                    let i = rng.below(buf.len() + 1);
                    buf.splice(i..i, tok.iter().copied());
                }
            }
            // Truncate at a random offset.
            6 if !buf.is_empty() => {
                buf.truncate(rng.below(buf.len() + 1));
            }
            // Append a random tail.
            7 => {
                let n = rng.below(1 + (max_len - buf.len()).min(64));
                for _ in 0..n {
                    buf.push(rng.byte());
                }
            }
            // Duplicate a span in place (amplification).
            8 if !buf.is_empty() && buf.len() < max_len => {
                let i = rng.below(buf.len());
                let n = 1 + rng.below(16.min(buf.len() - i));
                let span: Vec<u8> = buf[i..i + n].to_vec();
                let at = rng.below(buf.len() + 1);
                if buf.len() + n <= max_len {
                    buf.splice(at..at, span);
                }
            }
            // Fill a span with 0x80 (unterminated varints) or 0xFF.
            _ if !buf.is_empty() => {
                let i = rng.below(buf.len());
                let n = 1 + rng.below(16.min(buf.len() - i));
                let fill = *rng.pick(&[0x80u8, 0xff]);
                buf[i..i + n].fill(fill);
            }
            _ => {}
        }
    }
    buf.truncate(max_len);
    buf
}

/// Pathological standalone shapes the corpus may not reach: overlong varint
/// runs, u32 length bombs, and deep NBT nests.
fn bomb(rng: &mut Rng, max_len: usize) -> Vec<u8> {
    let mut out = Vec::new();
    match rng.below(8) {
        0 => out.extend(std::iter::repeat_n(0x80, 1 + rng.below(64))),
        1 => out.extend(std::iter::repeat_n(0xff, 1 + rng.below(64))),
        2 => {
            out.extend_from_slice(&u32::MAX.to_be_bytes());
            let n = rng.below(64);
            out.extend(std::iter::repeat_with(|| rng.byte()).take(n));
        }
        3 => {
            // VarInt(0x7FFFFFFF) length bomb + a dribble of payload.
            out.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0x07]);
            let n = rng.below(32);
            out.extend(std::iter::repeat_with(|| rng.byte()).take(n));
        }
        4 => {
            // Deeply nested anonymous-NBT compounds past the depth cap.
            let depth = 1 + rng.below(160);
            for _ in 0..depth {
                out.extend_from_slice(&[0x0a, 0x00, 0x00]);
            }
            out.push(0x00);
        }
        5 => {
            // NBT byte array with a 16 MiB element claim.
            out.extend_from_slice(&[0x07]);
            out.extend_from_slice(&((1 << 24) as u32).to_be_bytes());
            let n = rng.below(32);
            out.extend(std::iter::repeat_with(|| rng.byte()).take(n));
        }
        6 => {
            // NBT list of compounds with a huge count.
            out.extend_from_slice(&[0x09, 0x0a]);
            out.extend_from_slice(&u32::MAX.to_be_bytes());
            let n = rng.below(64);
            out.extend(std::iter::repeat_n(0x00, n));
        }
        _ => {
            let n = rng.below(max_len.min(512));
            out.extend(std::iter::repeat_with(|| rng.byte()).take(n));
        }
    }
    out
}

/// The next fuzz input: a corpus mutation, a boundary shape, a bomb, or raw
/// noise, weighted toward corpus-guided cases.
fn fuzz_input(rng: &mut Rng, corpus: &[Vec<u8>], max_len: usize) -> Vec<u8> {
    match rng.below(100) {
        // Mutated corpus seed.
        0..=44 => {
            let seed = rng.pick(corpus);
            mutate(rng, seed, max_len)
        }
        // Corpus seed truncated at a random offset, sometimes with tail.
        45..=59 => {
            let seed = rng.pick(corpus);
            let mut v = seed[..rng.below(seed.len() + 1)].to_vec();
            if rng.below(4) == 0 {
                let n = rng.below(24);
                v.extend(std::iter::repeat_with(|| rng.byte()).take(n));
            }
            v.truncate(max_len);
            v
        }
        // Splice of two corpus seeds.
        60..=69 => {
            let a = rng.pick(corpus);
            let b = rng.pick(corpus);
            let mut v = Vec::with_capacity(a.len() + b.len());
            v.extend_from_slice(&a[..rng.below(a.len() + 1)]);
            v.extend_from_slice(&b[..rng.below(b.len() + 1)]);
            v.truncate(max_len);
            v
        }
        // Bomb shapes.
        70..=79 => bomb(rng, max_len),
        // Raw noise (biased small, occasionally large).
        _ => {
            let n = if rng.below(16) == 0 {
                rng.below(max_len + 1)
            } else {
                rng.below(257)
            };
            std::iter::repeat_with(|| rng.byte()).take(n).collect()
        }
    }
}

// ---------------------------------------------------------------------------
// Corpus seeds — real packets encoded through the crate's own encoders, so
// mutations start from structures the decoders would actually accept.
// ---------------------------------------------------------------------------

const ALL_PHASES: [PacketPhase; 5] = [
    PacketPhase::Handshake,
    PacketPhase::Status,
    PacketPhase::Login,
    PacketPhase::Configuration,
    PacketPhase::Play,
];

fn c2s_samples() -> Vec<(PacketPhase, C2SPacket)> {
    vec![
        (
            PacketPhase::Handshake,
            C2SPacket::Handshake {
                protocol_version: 767,
                server_address: "localhost".into(),
                server_port: 25565,
                next_state: 2,
            },
        ),
        (
            PacketPhase::Login,
            C2SPacket::LoginStart {
                name: "Fuzzling".into(),
                uuid: [0x42; 16],
            },
        ),
        (
            PacketPhase::Login,
            C2SPacket::EncryptionResponse {
                shared_secret: vec![0x11; 32],
                verify_token: vec![0x22; 16],
            },
        ),
        (PacketPhase::Login, C2SPacket::LoginAcknowledged),
        (
            PacketPhase::Configuration,
            C2SPacket::ConfigKeepAlive { id: 42 },
        ),
        (
            PacketPhase::Configuration,
            C2SPacket::ConfigFinishConfiguration,
        ),
        (PacketPhase::Play, C2SPacket::KeepAlive { id: -7 }),
        (
            PacketPhase::Play,
            C2SPacket::MoveVehicle {
                x: 1.5,
                y: 64.0,
                z: -2.5,
                yaw: 90.0,
                pitch: 45.0,
            },
        ),
        (
            PacketPhase::Play,
            C2SPacket::PaddleBoat {
                left: true,
                right: false,
            },
        ),
        (
            PacketPhase::Play,
            C2SPacket::PlayerInput {
                sideways: 1.0,
                forward: -1.0,
                jump: true,
                unmount: false,
            },
        ),
        (
            PacketPhase::Play,
            C2SPacket::PlayerInputFlags {
                forward: true,
                backward: false,
                left: true,
                right: false,
                jump: true,
                shift: false,
                sprint: true,
            },
        ),
    ]
}

fn s2c_samples() -> Vec<(PacketPhase, S2CPacket)> {
    vec![
        (
            PacketPhase::Login,
            S2CPacket::LoginDisconnect {
                reason: text_component("nope"),
            },
        ),
        (
            PacketPhase::Login,
            S2CPacket::EncryptionRequest {
                server_id: String::new(),
                public_key: vec![0x30; 64],
                verify_token: vec![0x55; 16],
                should_authenticate: true,
            },
        ),
        (
            PacketPhase::Login,
            S2CPacket::LoginSuccess {
                uuid: [0x77; 16],
                username: "Fuzzling".into(),
                properties: vec![LoginProperty {
                    name: "textures".into(),
                    value: "AAAA".into(),
                    signature: Some("sig".into()),
                }],
            },
        ),
        (PacketPhase::Login, S2CPacket::SetCompression { threshold: 256 }),
        (
            PacketPhase::Configuration,
            S2CPacket::ConfigDisconnect {
                reason: text_component("bye"),
            },
        ),
        (
            PacketPhase::Configuration,
            S2CPacket::ConfigKeepAlive { id: 0xbeef },
        ),
        (
            PacketPhase::Configuration,
            S2CPacket::ConfigRegistryData {
                registry_id: "minecraft:dimension_type".into(),
                entries: vec![
                    RegistryDataEntry {
                        entry_id: "minecraft:overworld".into(),
                        data: Some(vec![0x0a, 0x00, 0x00, 0x00]),
                    },
                    RegistryDataEntry {
                        entry_id: "minecraft:the_nether".into(),
                        data: None,
                    },
                ],
            },
        ),
        (
            PacketPhase::Configuration,
            S2CPacket::ConfigFinishConfiguration,
        ),
        (
            PacketPhase::Play,
            S2CPacket::PlayDisconnect {
                reason: text_component("bye"),
            },
        ),
        (PacketPhase::Play, S2CPacket::KeepAlive { id: 0x1234 }),
        (
            PacketPhase::Play,
            S2CPacket::BlockUpdate {
                pos: BlockPos::new(-120, 64, 3_000),
                state_id: 8598,
            },
        ),
        (
            PacketPhase::Play,
            S2CPacket::ForgetLevelChunk { cx: -3, cz: 77 },
        ),
        (
            PacketPhase::Play,
            S2CPacket::LightUpdateMask {
                chunk_x: 4,
                chunk_z: -9,
                sky_mask: BitSet::from_u64(0b101),
                block_mask: BitSet::from_u64(0b010),
                empty_sky_mask: BitSet::from_u64(0b010),
                empty_block_mask: BitSet::from_u64(0b101),
                sky_light: vec![vec![0xff; 2048], vec![0x00; 2048]],
                block_light: vec![vec![0xaa; 2048]],
            },
        ),
        (
            PacketPhase::Play,
            S2CPacket::OpenScreen {
                container_id: 2,
                menu: MenuKind::Anvil,
                title: text_component("Repair & Name"),
            },
        ),
        (
            PacketPhase::Play,
            S2CPacket::SetEquipment {
                entity_id: 99,
                slots: vec![
                    EquipmentSlot {
                        slot: 0,
                        item: ItemStack {
                            item_id: 42,
                            count: 3,
                            added: vec![RawComponent {
                                type_id: 5,
                                data: vec![0x0a, 0x00, 0x00, 0x00],
                            }],
                            removed: vec![7],
                        },
                    },
                    EquipmentSlot {
                        slot: 5,
                        item: ItemStack::empty(),
                    },
                ],
            },
        ),
        (
            PacketPhase::Play,
            S2CPacket::SetPassengers {
                vehicle: 55,
                passengers: vec![100, -4, 0],
            },
        ),
        (
            PacketPhase::Play,
            S2CPacket::SetPlayerTeam {
                name: "fuzzers".into(),
                operation: TeamOperation::Create {
                    info: TeamInfo {
                        display_name: text_component("Fuzzers"),
                        friendly_flags: 3,
                        nametag_visibility: "always".into(),
                        collision_rule: "never".into(),
                        color: 14,
                        prefix: text_component("["),
                        suffix: text_component("]"),
                    },
                    members: vec!["a".into(), "b".into()],
                },
            },
        ),
        (
            PacketPhase::Play,
            S2CPacket::SetPlayerTeam {
                name: "fuzzers".into(),
                operation: TeamOperation::RemoveMembers {
                    members: vec!["a".into()],
                },
            },
        ),
        (
            PacketPhase::Play,
            S2CPacket::TabList {
                header: text_component("hi"),
                footer: text_component("bye"),
            },
        ),
        (
            PacketPhase::Play,
            S2CPacket::TickingState {
                tick_rate: 20.0,
                is_frozen: false,
            },
        ),
        (PacketPhase::Play, S2CPacket::TickingStep { steps: 3 }),
    ]
}

/// Real wire frames for both builtin tables, encoded by the codecs
/// themselves so every frame id is the table's own.
fn codec_frame_corpus() -> Vec<Vec<u8>> {
    let mut corpus = Vec::new();
    let mut c767 = CodecV767::new();
    let mut c777 = TableDrivenCodec::for_version(
        ProtocolVersion::V26_3,
        ProtocolTable::v777_builtin(),
    );
    for (phase, packet) in c2s_samples() {
        for codec in [&mut c767 as &mut dyn Codec, &mut c777] {
            codec.set_phase(phase);
            let mut frame = Vec::new();
            if codec.encode_c2s(&packet, &mut frame).is_ok() {
                corpus.push(frame);
            }
        }
    }
    for (phase, packet) in s2c_samples() {
        for codec in [&mut c767 as &mut dyn Codec, &mut c777] {
            codec.set_phase(phase);
            let mut frame = Vec::new();
            if codec.encode_s2c(&packet, &mut frame).is_ok() {
                corpus.push(frame);
            }
        }
    }
    corpus.extend(codec_edge_frames());
    corpus
}

/// The common phase-selecting surface of both builtin codecs, lifted into a
/// trait so the corpus builder and the fuzz loop treat them uniformly. The
/// `*_in` decodes are inherent methods on each codec; delegating here keeps
/// every decode entrypoint — `decode_c2s`/`decode_s2c` and the phase-explicit
/// `decode_c2s_in`/`decode_s2c_in` — reachable through one object.
trait Codec: ProtocolCodec {
    fn set_phase(&mut self, phase: PacketPhase);
    fn decode_c2s_in(&self, phase: PacketPhase, frame: &[u8]) -> Result<C2SPacket, mc_spine_proto::ProtoError>;
    fn decode_s2c_in(&self, phase: PacketPhase, frame: &[u8]) -> Result<S2CPacket, mc_spine_proto::ProtoError>;
}
impl Codec for CodecV767 {
    fn set_phase(&mut self, phase: PacketPhase) {
        CodecV767::set_phase(self, phase);
    }
    fn decode_c2s_in(&self, phase: PacketPhase, frame: &[u8]) -> Result<C2SPacket, mc_spine_proto::ProtoError> {
        CodecV767::decode_c2s_in(self, phase, frame)
    }
    fn decode_s2c_in(&self, phase: PacketPhase, frame: &[u8]) -> Result<S2CPacket, mc_spine_proto::ProtoError> {
        CodecV767::decode_s2c_in(self, phase, frame)
    }
}
impl Codec for TableDrivenCodec {
    fn set_phase(&mut self, phase: PacketPhase) {
        TableDrivenCodec::set_phase(self, phase);
    }
    fn decode_c2s_in(&self, phase: PacketPhase, frame: &[u8]) -> Result<C2SPacket, mc_spine_proto::ProtoError> {
        TableDrivenCodec::decode_c2s_in(self, phase, frame)
    }
    fn decode_s2c_in(&self, phase: PacketPhase, frame: &[u8]) -> Result<S2CPacket, mc_spine_proto::ProtoError> {
        TableDrivenCodec::decode_s2c_in(self, phase, frame)
    }
}

/// Trailing hand-rolled edge frames appended to the codec corpus.
fn codec_edge_frames() -> Vec<Vec<u8>> {
    let mut corpus = Vec::new();
    for (id, body) in [
        (0x00, &[][..]),
        (0x00, &[0x00][..]),
        (0x7f, &[][..]),
        (0x00, &[0xff, 0xff, 0xff, 0xff, 0x0f][..]),
    ] {
        let mut frame = Vec::new();
        write_frame(&mut frame, id, body);
        corpus.push(frame);
    }
    corpus
}

fn voice_corpus() -> Vec<Vec<u8>> {
    [
        VoicePacket::Auth {
            player_uuid: [1; 16],
            secret: [2; 16],
        },
        VoicePacket::AuthAck { ok: true },
        VoicePacket::MicFrame {
            player_uuid: [3; 16],
            seq: 7,
            whispering: false,
            opus_payload: vec![0xaa; 64],
        },
        VoicePacket::KeepAlive {
            player_uuid: [4; 16],
            nonce: 9,
        },
        VoicePacket::KeepAliveAck { nonce: 9 },
        VoicePacket::PlayerSound {
            sender_uuid: [5; 16],
            seq: 7,
            whispering: true,
            distance_sq: 144.0,
            group: false,
            opus_payload: vec![0xbb; 32],
        },
    ]
    .iter()
    .filter_map(|p| voice_relay::encode_packet(p).ok())
    .collect()
}

fn friend_corpus() -> Vec<Vec<u8>> {
    let code = InviteCode::from_bytes([0x11, 0x22, 0x33, 0x44, 0x55]);
    [
        FriendPacket::HostRegister {
            protocol_version: 767,
            world_name: "Fuzz World".into(),
            max_guests: 8,
            host_secret: [0x66; 16],
        },
        FriendPacket::HostRegistered { invite_code: code },
        FriendPacket::HostRevoke {
            invite_code: code,
            host_secret: [0x66; 16],
        },
        FriendPacket::HostRevoked { ok: false },
        FriendPacket::GuestJoin {
            invite_code: code,
            guest_name: "Steve".into(),
        },
        FriendPacket::GuestAccepted { stream_id: 42 },
        FriendPacket::GuestRejected {
            reason: "full".into(),
        },
        FriendPacket::StreamOpen {
            stream_id: 42,
            guest_name: "Steve".into(),
        },
        FriendPacket::StreamData {
            stream_id: 42,
            payload: b"handshake-ish".to_vec(),
        },
        FriendPacket::StreamClose {
            stream_id: 42,
            reason: "done".into(),
        },
        FriendPacket::Heartbeat { nonce: 123 },
        FriendPacket::HeartbeatAck { nonce: 123 },
    ]
    .iter()
    .filter_map(|p| friend_relay::encode_frame(p).ok())
    .collect()
}

fn invite_corpus() -> Vec<String> {
    vec![
        "MEGU-ABCD-EFGH".to_string(),
        "megu-abcd-efgh".to_string(),
        "MEGUABCDEFGH".to_string(),
        "ABCD-EFGH".to_string(),
        "01234567".to_string(),
        "O0oIL1lI".to_string(),
        InviteCode::from_bytes([9, 8, 7, 6, 5]).to_string(),
    ]
}

/// A valid DNS SRV reply a hostile server could plausibly mutate: mirrors
/// the fixture shape `resolver::dns::tests` uses — a CNAME record plus SRV
/// answers whose owner names are compression pointers into the question.
fn dns_reply_corpus() -> Vec<Vec<u8>> {
    // Minimal by-hand query-shaped header + question for
    // `_minecraft._tcp.fuzz.invalid.` (txid patched at send time).
    let mut query = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in [
        "_minecraft",
        "_tcp",
        "fuzz",
        "invalid",
    ] {
        query.push(label.len() as u8);
        query.extend_from_slice(label.as_bytes());
    }
    query.push(0);
    query.extend_from_slice(&33u16.to_be_bytes()); // SRV
    query.extend_from_slice(&1u16.to_be_bytes()); // IN

    // Valid reply: QR | RD | RA, one question, CNAME + two SRV answers.
    let mut reply = query.clone();
    reply[2] = 0x81;
    reply[3] = 0x80;
    reply[6..8].copy_from_slice(&3u16.to_be_bytes()); // 3 answers
    // CNAME record (type 5), owner = pointer to question name.
    reply.extend_from_slice(&[0xc0, 12, 0, 5, 0, 1, 0, 0, 0, 60, 0, 2, 0xc0, 12]);
    // Two SRV records; the second wins on lower priority.
    for (priority, weight, port, target) in [
        (10u16, 5u16, 25571u16, "backup.invalid."),
        (5u16, 1u16, 25570u16, "main.invalid."),
    ] {
        let mut rdata = priority.to_be_bytes().to_vec();
        rdata.extend_from_slice(&weight.to_be_bytes());
        rdata.extend_from_slice(&port.to_be_bytes());
        for label in target.trim_end_matches('.').split('.') {
            rdata.push(label.len() as u8);
            rdata.extend_from_slice(label.as_bytes());
        }
        rdata.push(0);
        reply.extend_from_slice(&[0xc0, 12, 0, 33, 0, 1, 0, 0, 0, 60]);
        reply.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        reply.extend_from_slice(&rdata);
    }
    vec![query, reply]
}

// ---------------------------------------------------------------------------
// Loopback DNS responder: answers `query_srv` with fuzzed reply bytes so the
// module-private `parse_answer`/`read_name` run on attacker-controlled data.
// Only `127.0.0.1` is ever touched; nothing leaves the host.
// ---------------------------------------------------------------------------

struct DnsResponder {
    addr: SocketAddr,
    replies: Arc<Mutex<VecDeque<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    udp_thread: Option<JoinHandle<()>>,
    tcp_thread: Option<JoinHandle<()>>,
}

impl DnsResponder {
    fn start() -> Option<Self> {
        let udp = UdpSocket::bind(("127.0.0.1", 0)).ok()?;
        udp.set_read_timeout(Some(Duration::from_millis(10))).ok()?;
        let addr = udp.local_addr().ok()?;
        // TCP on the same port number — the truncated-reply retry path.
        let tcp = TcpListener::bind(addr).ok();
        let replies: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let udp_thread = {
            let replies = Arc::clone(&replies);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                while !stop.load(Ordering::Relaxed) {
                    let Ok((n, from)) = udp.recv_from(&mut buf) else {
                        continue;
                    };
                    let reply = replies
                        .lock()
                        .ok()
                        .and_then(|mut queue| queue.pop_front());
                    if let Some(mut reply) = reply {
                        if n >= 2 && reply.len() >= 2 {
                            reply[..2].copy_from_slice(&buf[..2]);
                        }
                        let _ = udp.send_to(&reply, from);
                    }
                }
            })
        };

        let tcp_thread = tcp.map(|listener| {
            let replies = Arc::clone(&replies);
            let stop = Arc::clone(&stop);
            listener.set_nonblocking(true).ok();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let Ok((mut stream, _)) = listener.accept() else {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    };
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
                    let mut len = [0u8; 2];
                    if stream.read_exact(&mut len).is_err() {
                        continue;
                    }
                    let mut query = vec![0u8; u16::from_be_bytes(len) as usize];
                    if stream.read_exact(&mut query).is_err() {
                        continue;
                    }
                    let Ok(mut queue) = replies.lock() else {
                        continue;
                    };
                    let reply = queue.pop_front();
                    drop(queue);
                    if let Some(mut reply) = reply {
                        if query.len() >= 2 && reply.len() >= 2 {
                            reply[..2].copy_from_slice(&query[..2]);
                        }
                        let mut out = (reply.len() as u16).to_be_bytes().to_vec();
                        out.extend_from_slice(&reply);
                        let _ = stream.write_all(&out);
                    }
                }
            })
        });

        Some(Self {
            addr,
            replies,
            stop,
            udp_thread: Some(udp_thread),
            tcp_thread,
        })
    }

    fn queue_reply(&self, reply: Vec<u8>) {
        if let Ok(mut q) = self.replies.lock() {
            // A TCP retry can consume a second reply per query; keep the
            // queue shallow so stale replies never accumulate.
            q.push_back(reply);
            while q.len() > 4 {
                q.pop_front();
            }
        }
    }
}

impl Drop for DnsResponder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.udp_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.tcp_thread.take() {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------
// WireReader: every primitive, scripted in sequence so a mid-buffer valid
// prefix drives the deeper reads too.
// ---------------------------------------------------------------------------

#[test]
fn wire_reader_primitives_never_panic() {
    let mut rng = Rng::new(0x7769_7265_0001);
    let mut corpus: Vec<Vec<u8>> = codec_frame_corpus();
    corpus.extend(voice_corpus());
    corpus.extend(friend_corpus());
    // Hand-built primitive seeds.
    let mut w = WireWriter::new();
    w.write_varint(0);
    w.write_varint(-1);
    w.write_varint(i32::MAX);
    w.write_varlong(i64::MIN);
    w.write_string("fuzz-string \u{2603}");
    w.write_bitset(&BitSet::from_words(vec![0xdead_beef, 1]));
    w.write_bitset_bytes(&BitSet::from_u64(42));
    w.write_bytes(&[0xAB; 24]);
    corpus.push(w.into_vec());

    let iters = iters();
    let mut decoded = 0usize;
    for _ in 0..iters {
        let input = fuzz_input(&mut rng, &corpus, 64 * 1024);
        drive("WireReader::script", &input, || {
            let mut r = WireReader::new(&input);
            // Scripted sequence covering every untrusted primitive.
            let _ = r.read_varint();
            let _ = r.read_varlong();
            let _ = r.read_string(MAX_STRING_CHARS);
            let _ = r.read_bitset(4096);
            let _ = r.read_bitset_bytes(8192);
            let _ = r.read_uuid();
            let _ = r.read_position();
            let _ = r.read_anonymous_nbt();
            let _ = r.read_bytes(1 << 20);
            let _ = r.read_bool();
            let _ = r.finish();
        });
        // A second pass: pure varint sweep — the most common first read.
        drive("WireReader::varint_sweep", &input, || {
            let mut r = WireReader::new(&input);
            while r.read_varint().is_ok() {}
        });
        drive("WireReader::varlong_sweep", &input, || {
            let mut r = WireReader::new(&input);
            while r.read_varlong().is_ok() {}
        });
        drive("WireReader::nbt_sweep", &input, || {
            let mut r = WireReader::new(&input);
            while r.read_anonymous_nbt().is_ok() {}
        });
        decoded += 1;
    }
    eprintln!("wire_reader_primitives: {decoded} inputs x 4 scripts");
}

// ---------------------------------------------------------------------------
// The raw frame decoder (`VarInt(len) + VarInt(id) + payload`).
// ---------------------------------------------------------------------------

#[test]
fn frame_decoder_never_panics() {
    let mut rng = Rng::new(0x7769_7265_0002);
    let corpus = codec_frame_corpus();
    let iters = iters();
    let mut decoded = 0usize;
    for _ in 0..iters {
        let input = fuzz_input(&mut rng, &corpus, 64 * 1024);
        drive("read_frame", &input, || {
            let _ = read_frame(&input);
        });
        drive("read_frame_bounded", &input, || {
            for bound in [0usize, 1, 21, 1024, 16 * 1024 * 1024] {
                let _ = read_frame_bounded(&input, bound);
            }
        });
        decoded += 1;
    }
    eprintln!("frame_decoder: {decoded} inputs x 6 decoder calls");
}

// ---------------------------------------------------------------------------
// CodecV767 and TableDrivenCodec(777): every phase, both directions.
// ---------------------------------------------------------------------------

/// Hammer one codec object: every phase through the phase-explicit
/// `decode_*_in` entrypoints, plus the phaseful `decode_c2s`/`decode_s2c`
/// trait path after `set_phase`.
fn hammer_codec(
    target: &str,
    codec: &mut dyn Codec,
    corpus: &[Vec<u8>],
    iters: usize,
) -> usize {
    let mut rng = Rng::new(0xC0DE_C000 + target.len() as u64);
    let mut decoded = 0usize;
    for _ in 0..iters {
        let input = fuzz_input(&mut rng, corpus, 256 * 1024);
        for phase in ALL_PHASES {
            drive(target, &input, || {
                let _ = codec.decode_c2s_in(phase, &input);
            });
            drive(target, &input, || {
                let _ = codec.decode_s2c_in(phase, &input);
            });
            codec.set_phase(phase);
            drive(target, &input, || {
                let _ = codec.decode_c2s(&input);
                let _ = codec.decode_s2c(&input);
            });
        }
        decoded += 1;
    }
    decoded
}

#[test]
fn codec_v767_all_phases_never_panic() {
    let mut codec = CodecV767::new();
    let corpus = codec_frame_corpus();
    let n = hammer_codec("CodecV767", &mut codec, &corpus, iters());
    eprintln!("codec_v767: {n} inputs x 15 phase/direction decodes");
}

#[test]
fn table_driven_codec_v777_all_phases_never_panic() {
    let mut codec = TableDrivenCodec::for_version(
        ProtocolVersion::V26_3,
        ProtocolTable::v777_builtin(),
    );
    let corpus = codec_frame_corpus();
    let n = hammer_codec("TableDrivenCodec(777)", &mut codec, &corpus, iters());
    eprintln!("table_driven_codec_v777: {n} inputs x 15 phase/direction decodes");
}

// ---------------------------------------------------------------------------
// ServerSession::on_frame — sessions are pushed through real phase
// transitions, then fed fuzzed frames mid-flight.
// ---------------------------------------------------------------------------

fn session_config() -> ServerSessionConfig {
    ServerSessionConfig {
        router: std::sync::Arc::new(VersionRouter::with_v767_and_v777()),
        motd: "fuzz".into(),
        max_players: 20,
        compression_threshold: None,
        strict_version_negotiation: true,
    }
}

/// A valid handshake frame for `protocol` requesting `next_state`.
fn handshake_frame(protocol: i32, next_state: i32) -> Vec<u8> {
    let mut w = WireWriter::new();
    w.write_varint(protocol);
    w.write_string("localhost");
    w.write_u16(25565);
    w.write_varint(next_state);
    let mut frame = Vec::new();
    write_frame(&mut frame, 0x00, &w.into_vec());
    frame
}

/// Drive `session` as far toward Play as the real exchange allows, so fuzzed
/// frames land in Login/Configuration/Play too, not only Handshake.
fn advance_session(session: &mut ServerSession) {
    let _ = session.on_frame(&handshake_frame(767, 2));
    // login_start (id 0x00): name + uuid.
    let mut w = WireWriter::new();
    w.write_string("Fuzzling");
    w.write_uuid(&[0x42; 16]);
    let mut frame = Vec::new();
    write_frame(&mut frame, 0x00, &w.into_vec());
    if session.on_frame(&frame).is_err() {
        return;
    }
    if session.accept_login([0x42; 16], "Fuzzling", Vec::new()).is_err() {
        return;
    }
    // login_acknowledged (id 0x03, empty body) -> Configuration.
    let mut frame = Vec::new();
    write_frame(&mut frame, 0x03, &[]);
    if session.on_frame(&frame).is_err() {
        return;
    }
    // configuration finish (c2s id resolves through the table).
    let mut codec_body_frame = Vec::new();
    if let Some(codec) = session.codec() {
        let _ = codec.encode_c2s(&C2SPacket::ConfigFinishConfiguration, &mut codec_body_frame);
    }
    if !codec_body_frame.is_empty() {
        let _ = session.on_frame(&codec_body_frame);
    }
}

#[test]
fn server_session_on_frame_never_panics() {
    let mut rng = Rng::new(0x5e55_10a0_0001);
    let corpus = codec_frame_corpus();
    let config = session_config();
    let iters = iters();
    let mut decoded = 0usize;
    for i in 0..iters {
        let mut session = ServerSession::new(config.clone());
        // Three pre-state flavours: cold (no handshake), status, and the full
        // run toward Play — so every `on_*` handler sees hostile bytes.
        match i % 4 {
            0 => {}
            1 => {
                let _ = session.on_frame(&handshake_frame(767, 1));
            }
            2 => {
                let _ = session.on_frame(&handshake_frame(0xdead, 2));
            }
            _ => advance_session(&mut session),
        }
        let frames = 1 + rng.below(4);
        for _ in 0..frames {
            let input = fuzz_input(&mut rng, &corpus, 64 * 1024);
            drive("ServerSession::on_frame", &input, || {
                let _ = session.on_frame(&input);
            });
            decoded += 1;
        }
    }
    eprintln!("server_session: {decoded} frames over {iters} sessions");
}

// ---------------------------------------------------------------------------
// VCR1 voice datagrams.
// ---------------------------------------------------------------------------

#[test]
fn voice_relay_decode_packet_never_panics() {
    let mut rng = Rng::new(0x0C10_0000_0001);
    let corpus = voice_corpus();
    let iters = iters();
    let mut decoded = 0usize;
    for _ in 0..iters {
        let input = fuzz_input(&mut rng, &corpus, 2048);
        drive("voice_relay::decode_packet", &input, || {
            let _ = voice_relay::decode_packet(&input);
        });
        decoded += 1;
    }
    eprintln!("voice_relay: {decoded} datagrams");
}

// ---------------------------------------------------------------------------
// FRL1 friend-relay frames, bodies, and invite codes.
// ---------------------------------------------------------------------------

#[test]
fn friend_relay_decoders_and_invite_parse_never_panic() {
    let mut rng = Rng::new(0xFF11_0000_0001);
    let frame_corpus = friend_corpus();
    // decode_packet takes the body without the u32be length prefix.
    let body_corpus: Vec<Vec<u8>> = frame_corpus.iter().map(|f| f[4..].to_vec()).collect();
    let invites = invite_corpus();
    let iters = iters();
    let mut decoded = 0usize;
    for _ in 0..iters {
        let input = fuzz_input(&mut rng, &frame_corpus, 80 * 1024);
        drive("friend_relay::decode_frame", &input, || {
            let _ = friend_relay::decode_frame(&input);
        });
        let body = fuzz_input(&mut rng, &body_corpus, 80 * 1024);
        drive("friend_relay::decode_packet", &body, || {
            let _ = friend_relay::decode_packet(&body);
        });
        // Invite codes: mutate a real code's text and random ASCII soup.
        // Operate on chars so inserts always land on UTF-8 boundaries.
        let seed_text = match rng.below(4) {
            0..=2 => rng.pick(&invites).clone(),
            _ => String::new(),
        };
        let mut chars: Vec<char> = seed_text.chars().collect();
        for _ in 0..rng.below(8) {
            let c = rng.byte();
            let ch = if c.is_ascii() {
                c as char
            } else {
                char::from_u32(0x80 + u32::from(c)).unwrap_or('x')
            };
            match rng.below(3) {
                0 if !chars.is_empty() => {
                    chars.remove(rng.below(chars.len()));
                }
                1 if !chars.is_empty() => {
                    let at = rng.below(chars.len());
                    chars[at] = ch;
                }
                _ => chars.insert(rng.below(chars.len() + 1), ch),
            }
        }
        let text: String = chars.into_iter().collect();
        drive("InviteCode::parse", text.as_bytes(), || {
            let _ = InviteCode::parse(&text);
        });
        decoded += 1;
    }
    eprintln!("friend_relay: {decoded} frames+bodies+codes");
}

// ---------------------------------------------------------------------------
// DNS SRV reply parser, driven through `query_srv` over loopback UDP/TCP.
// ---------------------------------------------------------------------------

#[test]
fn dns_srv_reply_parser_never_panics() {
    let Some(responder) = DnsResponder::start() else {
        eprintln!("dns_srv: loopback UDP unavailable; skipping");
        return;
    };
    let mut rng = Rng::new(0xD15_0000_0001);
    let corpus = dns_reply_corpus();
    let servers = [responder.addr];
    // Bound each query below the module's own production timeout so the loop
    // stays fast even when a fuzzed reply is never sent. `system_name_servers`
    // is exercised too (resolv.conf read only — no network traffic).
    let timeout = resolver_dns::SERVER_TIMEOUT.min(Duration::from_millis(25));
    let _system_servers = resolver_dns::system_name_servers();
    let iters = iters().min(2_000); // loopback round-trips: keep wall time sane
    let mut decoded = 0usize;
    for _ in 0..iters {
        let input = fuzz_input(&mut rng, &corpus, 2_048);
        responder.queue_reply(input.clone());
        // A second queued reply feeds a possible TCP retry after a fuzzed
        // reply with the TC bit set.
        if rng.below(4) == 0 {
            responder.queue_reply(fuzz_input(&mut rng, &corpus, 2_048));
        }
        drive("resolver::dns::parse_reply", &input, || {
            let _ = resolver_dns::query_srv(
                "_minecraft._tcp.fuzz.invalid.",
                &servers,
                timeout,
            );
        });
        decoded += 1;
    }
    eprintln!("dns_srv_reply: {decoded} hostile replies");
}

// ---------------------------------------------------------------------------
// Exhaustive deterministic sweeps: every corpus seed truncated at EVERY
// offset and every single-byte value at the head — the cases a random loop
// can miss.
// ---------------------------------------------------------------------------

#[test]
fn corpus_exhaustive_truncation_and_bitflip_sweep() {
    let corpus = codec_frame_corpus();
    let codec = CodecV767::new();
    let mut decoded = 0usize;
    for seed in &corpus {
        // Every truncation length.
        for cut in 0..=seed.len() {
            let input = &seed[..cut];
            drive("sweep::truncate", input, || {
                let _ = read_frame(input);
                for phase in ALL_PHASES {
                    let _ = codec.decode_c2s_in(phase, input);
                    let _ = codec.decode_s2c_in(phase, input);
                }
            });
            decoded += 1;
        }
        // Every single-byte corruption at the first 48 offsets.
        for i in 0..seed.len().min(48) {
            for v in [0x00u8, 0x7f, 0x80, 0xff] {
                let mut input = seed.clone();
                input[i] = v;
                drive("sweep::bitflip", &input, || {
                    for phase in ALL_PHASES {
                        let _ = codec.decode_c2s_in(phase, &input);
                        let _ = codec.decode_s2c_in(phase, &input);
                    }
                });
                decoded += 1;
            }
        }
    }
    // Same sweeps for VCR1 + FRL1 corpora.
    for seed in voice_corpus().iter().chain(friend_corpus().iter()) {
        for cut in 0..=seed.len() {
            let input = &seed[..cut];
            drive("sweep::relay", input, || {
                let _ = voice_relay::decode_packet(input);
                let _ = friend_relay::decode_frame(input);
            });
            decoded += 1;
        }
        for i in 0..seed.len() {
            for v in [0x00u8, 0xff] {
                let mut input = seed.clone();
                input[i] = v;
                drive("sweep::relay_flip", &input, || {
                    let _ = voice_relay::decode_packet(&input);
                    let _ = friend_relay::decode_frame(&input);
                });
                decoded += 1;
            }
        }
    }
    eprintln!("exhaustive_sweep: {decoded} inputs");
}
