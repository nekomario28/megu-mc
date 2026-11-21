// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Port-forwarding-free friend invites: one human-readable `MEGU-XXXX-XXXX`
//! code, a compact `FRL1` stream-multiplexing wire protocol and a live TCP
//! relay that bridges a single host tunnel to many guest connections — the
//! e4mc / "Play with Friends" shape in pure `std`, with zero external crates.
//!
//! A player opens a singleplayer or LAN world, the integrated host dials the
//! relay outbound (no router port forwarding anywhere), registers the world
//! under a 16-byte `host_secret`, and receives an [`InviteCode`]. Friends type
//! that code into their client; the relay admits each guest up to the host's
//! `max_guests` cap and then multiplexes every guest's opaque Minecraft TCP
//! bytes over the one host tunnel as `stream_id`-tagged [`FriendPacket::StreamData`]
//! frames.
//!
//! ## `FRL1` wire shape
//!
//! Every connection — host tunnel and guest link alike — speaks the same
//! framed protocol:
//!
//! ```text
//! frame = u32be(body_len) + body          body_len in [5, MAX_FRL1_FRAME_BYTES]
//! body  = "FRL1" + tag(1) + fields
//! ```
//!
//! * `0x01 HostRegister`: `i32 protocol_version | string world_name |
//!   u8 max_guests | 16 host_secret` — host tunnel opens a session.
//! * `0x02 HostRegistered`: `5 invite_code` — relay answers the issued code.
//! * `0x03 HostRevoke`: `5 invite_code | 16 host_secret` — revoke the code and
//!   close every guest stream on it.
//! * `0x04 HostRevoked`: `bool ok` — `false` for an unknown code or a secret
//!   that did not match (compared in constant time).
//! * `0x05 GuestJoin`: `5 invite_code | string guest_name`.
//! * `0x06 GuestAccepted`: `u32 stream_id` — the guest now owns exactly this
//!   stream; every further frame it sends must carry it.
//! * `0x07 GuestRejected`: `string reason` — unknown/expired code, full world,
//!   or a host tunnel that is already gone; the link closes after this frame.
//! * `0x08 StreamOpen`: `u32 stream_id | string guest_name` — relay → host:
//!   a guest was admitted and its stream is live.
//! * `0x09 StreamData`: `u32 stream_id | payload(rest of frame)` —
//!   bidirectional opaque Minecraft TCP bytes, capped by the frame bound.
//! * `0x0A StreamClose`: `u32 stream_id | string reason` — either side ends a
//!   stream (guest quit, host kick, revoke, expiry); the peer gets a copy.
//! * `0x0B Heartbeat` / `0x0C HeartbeatAck`: `u64 nonce` echo liveness.
//!
//! The length prefix is validated **before** any allocation: an oversized or
//! truncated frame, foreign magic, an unknown tag, invalid UTF-8 or trailing
//! bytes all fail closed through [`ProtoError`]; nothing panics on
//! attacker-controlled input.
//!
//! ## Invite codes
//!
//! [`InviteCode`] is 40 bits of entropy rendered Crockford Base32
//! (`0123456789ABCDEFGHJKMNPQRSTVWXYZ` — `I`/`L`/`O`/`U` are absent so the
//! code cannot be misread) as `MEGU-XXXX-XXXX`. Parsing is deliberately
//! tolerant on surface details — case-insensitive, optional hyphens, optional
//! `MEGU` prefix, `o → 0`, `i/l → 1` — and fail-closed on anything else:
//! wrong digit count, a non-ASCII byte, or a character outside the alphabet
//! is an error, never a guess.
//!
//! ## Layers
//!
//! * [`FriendPacket`] + [`encode_packet`]/[`decode_packet`] +
//!   [`encode_frame`]/[`decode_frame`] — the `FRL1` codec.
//! * [`FriendRelayRouter`] — the sans-IO session router: host table keyed by
//!   invite code, `max_guests` enforcement, `Instant`-TTL invite expiry (no
//!   `SystemTime` anywhere — the caller supplies the clock), constant-time
//!   secret comparison on revoke, and per-host stream isolation so one guest
//!   can never inject bytes into another guest's stream or another host's
//!   session.
//! * [`TcpFriendRelay`] — the live relay: a non-blocking `TcpListener` plus
//!   one [`TcpFriendRelay::poll_once`] drain per tick that accepts links,
//!   parses frames and bridges payload bytes both directions, with
//!   [`FriendRelayMetrics`] counters for everything it admitted or refused.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use mc_spine_core::random::Xoroshiro128pp;

use crate::wire::{ProtoError, WireReader, WireWriter};

/// Magic bytes every `FRL1` frame body opens with. Foreign traffic sharing a
/// listener fails closed on this check.
pub const FRIEND_MAGIC: [u8; 4] = *b"FRL1";

/// Hard cap on one `FRL1` frame body (`"FRL1"` + tag + fields): 64 KiB. The
/// `u32` length prefix is checked against this bound before a single payload
/// byte is copied, so a forged prefix can never force a large allocation.
pub const MAX_FRL1_FRAME_BYTES: usize = 64 * 1024;

/// Largest opaque payload a [`FriendPacket::StreamData`] may carry: the frame
/// cap minus its 9 bytes of header (4 magic + 1 tag + 4 `stream_id`).
pub const MAX_STREAM_PAYLOAD_BYTES: usize = MAX_FRL1_FRAME_BYTES - 9;

/// Smallest legal frame body: magic + a tag byte.
const MIN_FRL1_BODY_BYTES: usize = FRIEND_MAGIC.len() + 1;

/// Vanilla-style UTF-16-unit cap applied to every `FRL1` string field
/// (`world_name`, `guest_name`, `reason`).
const MAX_FRL1_STRING_CHARS: usize = 256;

/// Human-readable prefix the invite code renders as: `MEGU-XXXX-XXXX`.
pub const INVITE_CODE_PREFIX: &str = "MEGU";

/// Default invite-code TTL: a session registered for an evening of play
/// stays joinable for eight hours, then expires without any host action.
pub const DEFAULT_INVITE_TTL: Duration = Duration::from_secs(8 * 60 * 60);

/// Crockford Base32 alphabet: digits+letters minus the visually ambiguous
/// `I`, `L`, `O` and `U`.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

const TAG_HOST_REGISTER: u8 = 0x01;
const TAG_HOST_REGISTERED: u8 = 0x02;
const TAG_HOST_REVOKE: u8 = 0x03;
const TAG_HOST_REVOKED: u8 = 0x04;
const TAG_GUEST_JOIN: u8 = 0x05;
const TAG_GUEST_ACCEPTED: u8 = 0x06;
const TAG_GUEST_REJECTED: u8 = 0x07;
const TAG_STREAM_OPEN: u8 = 0x08;
const TAG_STREAM_DATA: u8 = 0x09;
const TAG_STREAM_CLOSE: u8 = 0x0A;
const TAG_HEARTBEAT: u8 = 0x0B;
const TAG_HEARTBEAT_ACK: u8 = 0x0C;

/// Constant-time comparison for the 16-byte host secrets: every byte is
/// always touched so an attacker timing `HostRevoked` replies learns nothing
/// about how many prefix bytes were right.
fn secret_eq(a: &[u8; 16], b: &[u8; 16]) -> bool {
    let mut diff = 0u8;
    for i in 0..16 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// A 40-bit invite code, rendered `MEGU-XXXX-XXXX` in Crockford Base32.
///
/// Ordering and equality are the raw code's, so it keys `BTreeMap` session
/// tables directly; [`InviteCode::eq_ct`] is the constant-time comparison
/// used where a supplied code is checked against a stored one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InviteCode {
    /// The code as five big-endian bytes (top 40 bits of the draw).
    bytes: [u8; 5],
}

impl InviteCode {
    /// Wrap raw code bytes (the form the wire carries).
    pub fn from_bytes(bytes: [u8; 5]) -> Self {
        Self { bytes }
    }

    /// The code as five big-endian bytes.
    pub fn as_bytes(&self) -> [u8; 5] {
        self.bytes
    }

    fn from_value(value: u64) -> Self {
        debug_assert!(value < (1 << 40));
        Self {
            bytes: (value << 24).to_be_bytes()[..5].try_into().unwrap(),
        }
    }

    fn value(&self) -> u64 {
        u64::from_be_bytes([
            0,
            0,
            0,
            self.bytes[0],
            self.bytes[1],
            self.bytes[2],
            self.bytes[3],
            self.bytes[4],
        ])
    }

    /// Constant-time code comparison: always touches all five bytes.
    pub fn eq_ct(&self, other: &Self) -> bool {
        let mut diff = 0u8;
        for i in 0..5 {
            diff |= self.bytes[i] ^ other.bytes[i];
        }
        diff == 0
    }

    /// The eight Crockford Base32 digits (no prefix, no hyphens).
    fn digits(&self) -> [u8; 8] {
        let value = self.value();
        let mut out = [b'0'; 8];
        for (i, d) in out.iter_mut().enumerate() {
            *d = CROCKFORD[((value >> (35 - 5 * i)) & 31) as usize];
        }
        out
    }

    /// Tolerant but fail-closed parse. Accepted surface forms:
    /// `MEGU-XXXX-XXXX`, `MEGUXXXXXXXX`, `XXXX-XXXX`, `XXXXXXXX`, any case —
    /// plus the homoglyph normalizations `o → 0` and `i/l → 1`. Anything
    /// else — wrong digit count, non-ASCII text, a character outside the
    /// alphabet — is [`ProtoError::Malformed`], never a guess.
    pub fn parse(text: &str) -> Result<Self, ProtoError> {
        let mut norm = String::with_capacity(text.len());
        for ch in text.chars() {
            if ch == '-' {
                continue;
            }
            if !ch.is_ascii() {
                return Err(ProtoError::Malformed("invite code must be ASCII"));
            }
            norm.push(ch.to_ascii_uppercase());
        }
        let body = norm.strip_prefix(INVITE_CODE_PREFIX).unwrap_or(&norm);
        if body.len() != 8 {
            return Err(ProtoError::Malformed("invite code must be 8 base32 digits"));
        }
        let mut value = 0u64;
        for &b in body.as_bytes() {
            let digit = match b {
                b'O' => b'0',
                b'I' | b'L' => b'1',
                other => other,
            };
            let Some(idx) = CROCKFORD.iter().position(|&c| c == digit) else {
                return Err(ProtoError::Malformed("invalid invite code character"));
            };
            value = (value << 5) | idx as u64;
        }
        Ok(Self::from_value(value))
    }
}

impl fmt::Display for InviteCode {
    /// The canonical human form: `MEGU-XXXX-XXXX`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = self.digits();
        // The alphabet is pure ASCII, so both halves are valid UTF-8.
        let hi = std::str::from_utf8(&d[..4]).unwrap();
        let lo = std::str::from_utf8(&d[4..]).unwrap();
        write!(f, "{INVITE_CODE_PREFIX}-{hi}-{lo}")
    }
}

/// One decoded `FRL1` frame body. `HostRegister`, `HostRevoke`, `GuestJoin`,
/// `StreamData`, `StreamClose` and `Heartbeat` travel host/guest → relay;
/// `HostRegistered`, `HostRevoked`, `GuestAccepted`, `GuestRejected`,
/// `StreamOpen`, `StreamData`, `StreamClose` and `HeartbeatAck` travel relay →
/// host/guest. `StreamData` and `StreamClose` are deliberately bidirectional:
/// guest bytes ride to the host inside the same frame shape the host uses to
/// answer.
#[derive(Clone, Debug, PartialEq)]
pub enum FriendPacket {
    /// Host → relay: open a world-sharing session. `host_secret` is the
    /// revoke/kick credential, compared in constant time on use.
    HostRegister {
        /// Minecraft protocol number the host is serving (e.g. 767).
        protocol_version: i32,
        /// Display name friends will see in the relay logs.
        world_name: String,
        /// Maximum concurrent guest streams this session admits.
        max_guests: u8,
        /// 16-byte capability the host later proves ownership with.
        host_secret: [u8; 16],
    },
    /// Relay → host: the issued invite code for this session.
    HostRegistered {
        /// The code friends type in, rendered `MEGU-XXXX-XXXX`.
        invite_code: InviteCode,
    },
    /// Host → relay: revoke `invite_code` and close every guest stream on
    /// it. Succeeds only while `host_secret` matches in constant time.
    HostRevoke {
        /// The session being revoked.
        invite_code: InviteCode,
        /// The credential issued at `HostRegister`.
        host_secret: [u8; 16],
    },
    /// Relay → host: result of the latest `HostRevoke`. `false` means the
    /// code was unknown or the secret did not match — nothing changed.
    HostRevoked {
        /// `true` iff the code is now revoked and its guests are closed.
        ok: bool,
    },
    /// Guest → relay: join the world behind `invite_code`.
    GuestJoin {
        /// The code the friend shared.
        invite_code: InviteCode,
        /// Display name forwarded to the host in `StreamOpen`.
        guest_name: String,
    },
    /// Relay → guest: admission granted; the link now speaks exclusively on
    /// `stream_id`.
    GuestAccepted {
        /// The stream this guest owns for the life of the link.
        stream_id: u32,
    },
    /// Relay → guest: admission refused; the link closes after this frame.
    GuestRejected {
        /// Why the join failed (`unknown invite`, `invite expired`,
        /// `world is full`, `host unavailable`).
        reason: String,
    },
    /// Relay → host: a guest was admitted and `stream_id` is live.
    StreamOpen {
        /// The admitted stream.
        stream_id: u32,
        /// The name the guest joined with.
        guest_name: String,
    },
    /// Either direction: `payload` is opaque Minecraft TCP bytes for
    /// `stream_id`, forwarded verbatim.
    StreamData {
        /// The stream these bytes belong to.
        stream_id: u32,
        /// Up to [`MAX_STREAM_PAYLOAD_BYTES`] of tunnel bytes.
        payload: Vec<u8>,
    },
    /// Either direction: end `stream_id`. The relay forwards the close to
    /// the stream's other end and forgets the stream.
    StreamClose {
        /// The stream being ended.
        stream_id: u32,
        /// Human-readable cause (`guest quit`, `host kick`, `invite revoked`).
        reason: String,
    },
    /// Either side → relay: liveness probe.
    Heartbeat {
        /// Opaque token echoed in `HeartbeatAck`.
        nonce: u64,
    },
    /// Relay → either side: liveness answer echoing the probe's nonce.
    HeartbeatAck {
        /// The nonce from the `Heartbeat` being answered.
        nonce: u64,
    },
}

/// Encode-side bound for `FRL1` string fields, mirroring the UTF-16-unit cap
/// [`WireReader::read_string`] enforces on decode.
fn check_string(s: &str) -> Result<(), ProtoError> {
    let units: usize = s.chars().map(char::len_utf16).sum();
    if units > MAX_FRL1_STRING_CHARS {
        return Err(ProtoError::StringTooLong {
            units,
            max: MAX_FRL1_STRING_CHARS,
        });
    }
    Ok(())
}

/// Encode one packet into an `FRL1` frame body (`"FRL1"` + tag + fields).
/// Fails closed — rather than truncating — when the result would exceed
/// [`MAX_FRL1_FRAME_BYTES`] or a field exceeds its own bound.
pub fn encode_packet(packet: &FriendPacket) -> Result<Vec<u8>, ProtoError> {
    let mut out = Vec::with_capacity(64);
    encode_packet_into(packet, &mut out)?;
    Ok(out)
}

/// [`encode_packet`] appending to a caller-owned buffer. On error `out` is
/// left exactly as it was.
pub fn encode_packet_into(packet: &FriendPacket, out: &mut Vec<u8>) -> Result<(), ProtoError> {
    let start = out.len();
    let mut w = WireWriter::from_vec(std::mem::take(out));
    w.raw(&FRIEND_MAGIC);
    let result = encode_packet_body(packet, &mut w);
    *out = w.into_vec();
    let len = out.len() - start;
    if let Err(e) = result {
        out.truncate(start);
        return Err(e);
    }
    if len > MAX_FRL1_FRAME_BYTES {
        out.truncate(start);
        return Err(ProtoError::FrameTooLarge {
            length: len,
            max: MAX_FRL1_FRAME_BYTES,
        });
    }
    Ok(())
}

/// The `FRL1` body (tag + fields) after the magic.
fn encode_packet_body(packet: &FriendPacket, w: &mut WireWriter) -> Result<(), ProtoError> {
    match packet {
        FriendPacket::HostRegister {
            protocol_version,
            world_name,
            max_guests,
            host_secret,
        } => {
            check_string(world_name)?;
            w.write_u8(TAG_HOST_REGISTER);
            w.write_i32(*protocol_version);
            w.write_string(world_name);
            w.write_u8(*max_guests);
            w.write_uuid(host_secret);
        }
        FriendPacket::HostRegistered { invite_code } => {
            w.write_u8(TAG_HOST_REGISTERED);
            w.raw(&invite_code.as_bytes());
        }
        FriendPacket::HostRevoke {
            invite_code,
            host_secret,
        } => {
            w.write_u8(TAG_HOST_REVOKE);
            w.raw(&invite_code.as_bytes());
            w.write_uuid(host_secret);
        }
        FriendPacket::HostRevoked { ok } => {
            w.write_u8(TAG_HOST_REVOKED);
            w.write_bool(*ok);
        }
        FriendPacket::GuestJoin {
            invite_code,
            guest_name,
        } => {
            check_string(guest_name)?;
            w.write_u8(TAG_GUEST_JOIN);
            w.raw(&invite_code.as_bytes());
            w.write_string(guest_name);
        }
        FriendPacket::GuestAccepted { stream_id } => {
            w.write_u8(TAG_GUEST_ACCEPTED);
            w.write_u32(*stream_id);
        }
        FriendPacket::GuestRejected { reason } => {
            check_string(reason)?;
            w.write_u8(TAG_GUEST_REJECTED);
            w.write_string(reason);
        }
        FriendPacket::StreamOpen {
            stream_id,
            guest_name,
        } => {
            check_string(guest_name)?;
            w.write_u8(TAG_STREAM_OPEN);
            w.write_u32(*stream_id);
            w.write_string(guest_name);
        }
        FriendPacket::StreamData {
            stream_id,
            payload,
        } => {
            if payload.len() > MAX_STREAM_PAYLOAD_BYTES {
                return Err(ProtoError::LengthOutOfBounds {
                    length: payload.len(),
                    max: MAX_STREAM_PAYLOAD_BYTES,
                });
            }
            w.write_u8(TAG_STREAM_DATA);
            w.write_u32(*stream_id);
            w.raw(payload);
        }
        FriendPacket::StreamClose { stream_id, reason } => {
            check_string(reason)?;
            w.write_u8(TAG_STREAM_CLOSE);
            w.write_u32(*stream_id);
            w.write_string(reason);
        }
        FriendPacket::Heartbeat { nonce } => {
            w.write_u8(TAG_HEARTBEAT);
            w.write_u64(*nonce);
        }
        FriendPacket::HeartbeatAck { nonce } => {
            w.write_u8(TAG_HEARTBEAT_ACK);
            w.write_u64(*nonce);
        }
    }
    Ok(())
}

/// Decode one `FRL1` frame body (no length prefix). Every failure mode —
/// oversize, truncation, foreign magic, unknown tag, forged length, invalid
/// UTF-8, trailing garbage — is a [`ProtoError`], never a panic.
pub fn decode_packet(body: &[u8]) -> Result<FriendPacket, ProtoError> {
    if body.len() > MAX_FRL1_FRAME_BYTES {
        return Err(ProtoError::FrameTooLarge {
            length: body.len(),
            max: MAX_FRL1_FRAME_BYTES,
        });
    }
    let mut r = WireReader::new(body);
    if r.take(FRIEND_MAGIC.len())? != FRIEND_MAGIC.as_slice() {
        return Err(ProtoError::Malformed("not a FRL1 frame body"));
    }
    let packet = match r.read_u8()? {
        TAG_HOST_REGISTER => FriendPacket::HostRegister {
            protocol_version: r.read_i32()?,
            world_name: r.read_string(MAX_FRL1_STRING_CHARS)?,
            max_guests: r.read_u8()?,
            host_secret: r.read_uuid()?,
        },
        TAG_HOST_REGISTERED => FriendPacket::HostRegistered {
            invite_code: InviteCode::from_bytes(r.take(5)?.try_into().unwrap()),
        },
        TAG_HOST_REVOKE => FriendPacket::HostRevoke {
            invite_code: InviteCode::from_bytes(r.take(5)?.try_into().unwrap()),
            host_secret: r.read_uuid()?,
        },
        TAG_HOST_REVOKED => FriendPacket::HostRevoked { ok: r.read_bool()? },
        TAG_GUEST_JOIN => FriendPacket::GuestJoin {
            invite_code: InviteCode::from_bytes(r.take(5)?.try_into().unwrap()),
            guest_name: r.read_string(MAX_FRL1_STRING_CHARS)?,
        },
        TAG_GUEST_ACCEPTED => FriendPacket::GuestAccepted {
            stream_id: r.read_u32()?,
        },
        TAG_GUEST_REJECTED => FriendPacket::GuestRejected {
            reason: r.read_string(MAX_FRL1_STRING_CHARS)?,
        },
        TAG_STREAM_OPEN => FriendPacket::StreamOpen {
            stream_id: r.read_u32()?,
            guest_name: r.read_string(MAX_FRL1_STRING_CHARS)?,
        },
        TAG_STREAM_DATA => {
            let stream_id = r.read_u32()?;
            let payload_len = r.remaining();
            FriendPacket::StreamData {
                stream_id,
                payload: r.take(payload_len)?.to_vec(),
            }
        }
        TAG_STREAM_CLOSE => FriendPacket::StreamClose {
            stream_id: r.read_u32()?,
            reason: r.read_string(MAX_FRL1_STRING_CHARS)?,
        },
        TAG_HEARTBEAT => FriendPacket::Heartbeat {
            nonce: r.read_u64()?,
        },
        TAG_HEARTBEAT_ACK => FriendPacket::HeartbeatAck {
            nonce: r.read_u64()?,
        },
        other => {
            return Err(ProtoError::InvalidDiscriminant {
                value: i64::from(other),
                what: "FRL1 packet tag",
            })
        }
    };
    r.finish()?;
    Ok(packet)
}

/// If `inbuf` holds a complete `u32be(len) + body` frame, return the total
/// frame length in bytes (`Ok(Some(n))`); `Ok(None)` when more bytes are
/// needed. The declared length is validated before the body is touched, so
/// an oversized or degenerate prefix is an error immediately — fail closed
/// on the 4-byte header alone.
fn next_frame_len(inbuf: &[u8]) -> Result<Option<usize>, ProtoError> {
    if inbuf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes(inbuf[..4].try_into().unwrap()) as usize;
    if !(MIN_FRL1_BODY_BYTES..=MAX_FRL1_FRAME_BYTES).contains(&len) {
        return Err(ProtoError::FrameTooLarge {
            length: len,
            max: MAX_FRL1_FRAME_BYTES,
        });
    }
    if inbuf.len() < 4 + len {
        return Ok(None);
    }
    Ok(Some(4 + len))
}

/// Encode one packet as a complete wire frame: `u32be(len) + "FRL1" + tag +
/// fields`.
pub fn encode_frame(packet: &FriendPacket) -> Result<Vec<u8>, ProtoError> {
    let mut out = Vec::new();
    encode_frame_into(packet, &mut out)?;
    Ok(out)
}

/// [`encode_frame`] appending to a caller-owned buffer: the fixed-width
/// length prefix is patched in place, so no body `Vec` is ever materialized.
/// On error `out` is left exactly as it was.
pub fn encode_frame_into(packet: &FriendPacket, out: &mut Vec<u8>) -> Result<(), ProtoError> {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    if let Err(e) = encode_packet_into(packet, out) {
        out.truncate(start);
        return Err(e);
    }
    let body_len = (out.len() - start - 4) as u32;
    out[start..start + 4].copy_from_slice(&body_len.to_be_bytes());
    Ok(())
}

/// Decode one complete wire frame. The declared length must match the
/// buffer exactly: truncation and trailing bytes both fail closed.
pub fn decode_frame(frame: &[u8]) -> Result<FriendPacket, ProtoError> {
    match next_frame_len(frame)? {
        Some(total) if total == frame.len() => decode_packet(&frame[4..]),
        Some(_) => Err(ProtoError::Malformed("trailing bytes after FRL1 frame")),
        None => Err(ProtoError::UnexpectedEof {
            needed: 4,
            remaining: frame.len().saturating_sub(4),
        }),
    }
}

/// Win32 CSPRNG for invite-code seeds: `BCryptGenRandom` with
/// `BCRYPT_USE_SYSTEM_PREFERRED_RNG` — the OS CNG generator, the same
/// entropy source `rand`/`getrandom` consumers use on Windows. `false`
/// on any API failure so the caller falls back to the portable mixer.
#[cfg(windows)]
fn windows_csprng(buf: &mut [u8]) -> bool {
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;

    #[link(name = "bcrypt")]
    extern "system" {
        fn BCryptGenRandom(
            hAlgorithm: *mut std::ffi::c_void,
            pbBuffer: *mut u8,
            cbBuffer: u32,
            dwFlags: u32,
        ) -> i32;
    }

    let Ok(len) = u32::try_from(buf.len()) else {
        return false;
    };
    // NTSTATUS: STATUS_SUCCESS == 0. `hAlgorithm` must be NULL when
    // BCRYPT_USE_SYSTEM_PREFERRED_RNG selects the system generator.
    unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            len,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        ) == 0
    }
}

/// OS entropy for invite-code generation: the platform CSPRNG —
/// `/dev/urandom` on Unix, `BCryptGenRandom` on Windows — with a portable
/// mixer (pid, heap ASLR and monotonic-clock jitter folded through a
/// splitmix64 finalizer) for platforms where neither is available.
fn draw_seed() -> u64 {
    let mut buf = [0u8; 8];
    #[cfg(unix)]
    {
        if std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut buf))
            .is_ok()
        {
            return u64::from_ne_bytes(buf);
        }
    }
    #[cfg(windows)]
    {
        if windows_csprng(&mut buf) {
            return u64::from_ne_bytes(buf);
        }
    }
    let marker = Box::new(0u8);
    let mut x = (u64::from(std::process::id()) << 32)
        ^ (&*marker as *const u8 as usize) as u64
        ^ 0x9E37_79B9_7F4A_7C15;
    let t0 = Instant::now();
    for _ in 0..256 {
        x ^= t0.elapsed().as_nanos() as u64;
        x ^= x >> 30;
        x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 31;
    }
    x
}

/// Tunables for [`FriendRelayRouter`].
#[derive(Clone, Debug, PartialEq)]
pub struct FriendRelayConfig {
    /// How long an issued invite code admits guests, measured against the
    /// caller-supplied [`Instant`] — never `SystemTime`, so an expiry can
    /// never be caused by a wall-clock correction.
    pub invite_ttl: Duration,
}

impl Default for FriendRelayConfig {
    fn default() -> Self {
        Self {
            invite_ttl: DEFAULT_INVITE_TTL,
        }
    }
}

/// Everything the router knows about one registered host session.
#[derive(Clone, Debug)]
pub struct HostSession {
    /// The invite code this session registered under (the table key).
    pub invite_code: InviteCode,
    /// Minecraft protocol number the host declared.
    pub protocol_version: i32,
    /// Display name the host registered.
    pub world_name: String,
    /// Concurrent guest cap; joins beyond it are rejected.
    pub max_guests: u8,
    /// Revoke/kick credential, compared in constant time.
    pub host_secret: [u8; 16],
    /// When the session registered, on the caller's clock.
    pub registered_at: Instant,
    stream_ids: BTreeSet<u32>,
}

impl HostSession {
    /// Live guest streams on this session.
    pub fn stream_count(&self) -> usize {
        self.stream_ids.len()
    }

    /// The stream ids currently open on this session.
    pub fn stream_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.stream_ids.iter().copied()
    }

    /// `true` iff `stream_id` belongs to this host's session.
    pub fn owns_stream(&self, stream_id: u32) -> bool {
        self.stream_ids.contains(&stream_id)
    }
}

/// One admitted guest stream: a globally unique `stream_id` bound to exactly
/// one host session and one guest link.
#[derive(Clone, Debug, PartialEq)]
pub struct GuestStream {
    /// The stream id both ends frame their bytes with.
    pub stream_id: u32,
    /// The host session this stream belongs to — the isolation boundary the
    /// router enforces on every `StreamData`/`StreamClose`.
    pub host_code: InviteCode,
    /// The name the guest joined with.
    pub guest_name: String,
}

/// Why [`FriendRelayRouter::revoke`] refused a `HostRevoke`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevokeError {
    /// No live session is registered under the supplied code.
    UnknownCode,
    /// The supplied `host_secret` did not match (constant-time compare).
    BadSecret,
}

/// A session [`FriendRelayRouter::sweep_expired`] just retired: its code plus
/// the guest stream ids that must now be closed.
#[derive(Clone, Debug, PartialEq)]
pub struct ExpiredHost {
    /// The invite code that lapsed.
    pub invite_code: InviteCode,
    /// Guest streams that were open on it.
    pub stream_ids: Vec<u32>,
}

/// Counters for everything the relay admitted or refused. `active_*` are
/// live counts read off the tables; the rest are cumulative.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FriendRelayMetrics {
    /// Registered host sessions right now.
    pub active_hosts: u64,
    /// Open guest streams right now.
    pub active_guests: u64,
    /// `StreamData` payload bytes forwarded in both directions.
    pub bytes_relayed: u64,
    /// `GuestJoin`s refused (bad code, expiry, cap, missing host).
    pub rejected_joins: u64,
    /// Codes revoked — explicitly via `HostRevoke`, by host disconnect or
    /// by invite expiry.
    pub revoked_codes: u64,
}

/// The sans-IO session router: invite-code → host table, guest stream table
/// and the admission/revocation/expiry rules. Pure — no sockets, no clock of
/// its own — so every rule is exercised deterministically by feeding it
/// state and [`Instant`]s.
pub struct FriendRelayRouter {
    config: FriendRelayConfig,
    hosts: BTreeMap<InviteCode, HostSession>,
    streams: BTreeMap<u32, GuestStream>,
    next_stream_id: u32,
    rng: Xoroshiro128pp,
    rejected_joins: u64,
    bytes_relayed: u64,
    revoked_codes: u64,
}

impl FriendRelayRouter {
    /// A router seeded from OS entropy (the platform CSPRNG).
    pub fn new(config: FriendRelayConfig) -> Self {
        Self::with_seed(config, draw_seed())
    }

    /// A router whose invite codes are drawn from `seed` — deterministic,
    /// for tests and reproducible sessions.
    pub fn with_seed(config: FriendRelayConfig, seed: u64) -> Self {
        Self {
            config,
            hosts: BTreeMap::new(),
            streams: BTreeMap::new(),
            next_stream_id: 1,
            rng: Xoroshiro128pp::new(seed),
            rejected_joins: 0,
            bytes_relayed: 0,
            revoked_codes: 0,
        }
    }

    /// The live configuration.
    pub fn config(&self) -> &FriendRelayConfig {
        &self.config
    }

    /// Read one host session.
    pub fn host(&self, code: &InviteCode) -> Option<&HostSession> {
        self.hosts.get(code)
    }

    /// Read one guest stream.
    pub fn stream(&self, stream_id: u32) -> Option<&GuestStream> {
        self.streams.get(&stream_id)
    }

    /// `true` iff `stream_id` exists **and** belongs to the session
    /// registered under `code` — the per-host isolation check the relay
    /// applies to every host-originated `StreamData`/`StreamClose`.
    pub fn stream_host_ok(&self, code: &InviteCode, stream_id: u32) -> bool {
        self.streams
            .get(&stream_id)
            .map(|s| s.host_code.eq_ct(code))
            .unwrap_or(false)
    }

    fn expired(&self, host: &HostSession, now: Instant) -> bool {
        now.saturating_duration_since(host.registered_at) >= self.config.invite_ttl
    }

    fn draw_invite_code(&mut self) -> InviteCode {
        loop {
            let code = InviteCode::from_value((self.rng.next_long() as u64) & ((1 << 40) - 1));
            if !self.hosts.contains_key(&code) {
                return code;
            }
        }
    }

    fn alloc_stream_id(&mut self) -> u32 {
        loop {
            let id = self.next_stream_id;
            self.next_stream_id = self.next_stream_id.wrapping_add(1).max(1);
            if !self.streams.contains_key(&id) {
                return id;
            }
        }
    }

    /// Register a host session and return the issued invite code. `now` is
    /// stamped as `registered_at`, the instant `invite_ttl` is measured from.
    pub fn register_host(
        &mut self,
        now: Instant,
        protocol_version: i32,
        world_name: &str,
        max_guests: u8,
        host_secret: [u8; 16],
    ) -> InviteCode {
        let invite_code = self.draw_invite_code();
        self.hosts.insert(
            invite_code,
            HostSession {
                invite_code,
                protocol_version,
                world_name: world_name.to_owned(),
                max_guests,
                host_secret,
                registered_at: now,
                stream_ids: BTreeSet::new(),
            },
        );
        invite_code
    }

    /// Attempt a `GuestJoin`: admit under `code` and allocate the stream.
    /// Order of the fail-closed rules — each bumps `rejected_joins`:
    ///
    /// 1. unknown or revoked code → `unknown invite`;
    /// 2. `invite_ttl` elapsed since registration → `invite expired`;
    /// 3. `max_guests` already reached → `world is full`.
    pub fn guest_join(
        &mut self,
        now: Instant,
        code: &InviteCode,
        guest_name: &str,
    ) -> Result<u32, String> {
        let Some(host) = self.hosts.get(code) else {
            self.rejected_joins += 1;
            return Err("unknown invite".to_owned());
        };
        if self.expired(host, now) {
            self.rejected_joins += 1;
            return Err("invite expired".to_owned());
        }
        if host.stream_ids.len() >= usize::from(host.max_guests) {
            self.rejected_joins += 1;
            return Err("world is full".to_owned());
        }
        let stream_id = self.alloc_stream_id();
        self.hosts
            .get_mut(code)
            .expect("host checked above")
            .stream_ids
            .insert(stream_id);
        self.streams.insert(
            stream_id,
            GuestStream {
                stream_id,
                host_code: *code,
                guest_name: guest_name.to_owned(),
            },
        );
        Ok(stream_id)
    }

    /// Attempt a `HostRevoke`: constant-time comparisons of the supplied
    /// code and secret, and on success the whole session — code plus every
    /// open guest stream — is torn down. Returns the stream ids the caller
    /// must close.
    pub fn revoke(
        &mut self,
        code: &InviteCode,
        host_secret: &[u8; 16],
    ) -> Result<Vec<u32>, RevokeError> {
        let Some(host) = self.hosts.get(code) else {
            return Err(RevokeError::UnknownCode);
        };
        let ok = host.invite_code.eq_ct(code) & secret_eq(&host.host_secret, host_secret);
        if !ok {
            return Err(RevokeError::BadSecret);
        }
        Ok(self.unregister_host(code).unwrap_or_default())
    }

    /// Tear a session down without its secret — the host tunnel dropped, so
    /// the code must die with it. Returns the guest stream ids that were
    /// open, or `None` if the code was already gone.
    pub fn unregister_host(&mut self, code: &InviteCode) -> Option<Vec<u32>> {
        let host = self.hosts.remove(code)?;
        let ids: Vec<u32> = host.stream_ids.iter().copied().collect();
        for id in &ids {
            self.streams.remove(id);
        }
        self.revoked_codes += 1;
        Some(ids)
    }

    /// End one stream and report which host session it belonged to so the
    /// caller can forward the close. `None` for unknown or already-closed
    /// ids — idempotent on the reap path.
    pub fn close_stream(&mut self, stream_id: u32) -> Option<InviteCode> {
        let stream = self.streams.remove(&stream_id)?;
        if let Some(host) = self.hosts.get_mut(&stream.host_code) {
            host.stream_ids.remove(&stream_id);
        }
        Some(stream.host_code)
    }

    /// Retire every session whose `invite_ttl` has elapsed at `now`.
    /// Returns the affected codes and their open stream ids so the caller
    /// can close the corresponding links.
    pub fn sweep_expired(&mut self, now: Instant) -> Vec<ExpiredHost> {
        let lapsed: Vec<InviteCode> = self
            .hosts
            .values()
            .filter(|h| self.expired(h, now))
            .map(|h| h.invite_code)
            .collect();
        let mut out = Vec::with_capacity(lapsed.len());
        for code in lapsed {
            if let Some(stream_ids) = self.unregister_host(&code) {
                out.push(ExpiredHost {
                    invite_code: code,
                    stream_ids,
                });
            }
        }
        out
    }

    /// Account forwarded `StreamData` payload bytes.
    pub fn note_relayed(&mut self, bytes: u64) {
        self.bytes_relayed += bytes;
    }

    /// Snapshot of the counters: live session/guest counts plus cumulative
    /// rejection, byte and revocation totals.
    pub fn metrics(&self) -> FriendRelayMetrics {
        FriendRelayMetrics {
            active_hosts: self.hosts.len() as u64,
            active_guests: self.streams.len() as u64,
            bytes_relayed: self.bytes_relayed,
            rejected_joins: self.rejected_joins,
            revoked_codes: self.revoked_codes,
        }
    }
}

/// What one accepted connection currently is.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ConnRole {
    /// `FRL1` link before `HostRegister`/`GuestJoin` identifies it.
    Handshake,
    /// A host tunnel carrying the multiplexed `stream_id` frames.
    Host { code: InviteCode },
    /// An admitted guest bound to exactly one `stream_id`.
    Guest { stream_id: u32 },
}

struct RelayConn {
    sock: TcpStream,
    role: ConnRole,
    inbuf: Vec<u8>,
    outbuf: Vec<u8>,
    /// Drop at the next reap point (EOF, malformed frame, isolation
    /// violation) — buffered output is discarded because the peer is gone
    /// or the link is untrusted.
    dead: bool,
    /// Flush `outbuf`, then drop — used for `GuestRejected`, `StreamClose`
    /// delivery and other orderly shutdowns.
    close_after_flush: bool,
}

/// The live relay: a non-blocking `TcpListener` in front of a
/// [`FriendRelayRouter`]. `poll_once` drains accepts, reads, frames and
/// flushes once, so a caller ticking once per game-loop iteration keeps
/// tunnel latency at one drain (sub-millisecond on loopback).
pub struct TcpFriendRelay {
    listener: TcpListener,
    router: FriendRelayRouter,
    conns: BTreeMap<u64, RelayConn>,
    next_conn_id: u64,
    host_conns: BTreeMap<InviteCode, u64>,
    stream_conns: BTreeMap<u32, u64>,
}

impl TcpFriendRelay {
    /// Bind `bind_addr` (e.g. `127.0.0.1:0` for an ephemeral port) with a
    /// fresh OS-seeded router.
    pub fn bind(bind_addr: SocketAddr) -> io::Result<Self> {
        Self::bind_with_router(bind_addr, FriendRelayRouter::new(FriendRelayConfig::default()))
    }

    /// Bind `bind_addr` serving a caller-built router — e.g. a
    /// [`FriendRelayRouter::with_seed`] for deterministic tests or a custom
    /// `invite_ttl`.
    pub fn bind_with_router(
        bind_addr: SocketAddr,
        router: FriendRelayRouter,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(bind_addr)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            router,
            conns: BTreeMap::new(),
            next_conn_id: 1,
            host_conns: BTreeMap::new(),
            stream_conns: BTreeMap::new(),
        })
    }

    /// The address this relay is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// The session router (read-only).
    pub fn router(&self) -> &FriendRelayRouter {
        &self.router
    }

    /// The session router for the game layer to drive directly.
    pub fn router_mut(&mut self) -> &mut FriendRelayRouter {
        &mut self.router
    }

    /// Snapshot of the relay counters.
    pub fn metrics(&self) -> FriendRelayMetrics {
        self.router.metrics()
    }

    /// One non-blocking relay step: drain the accept queue, drain every
    /// conn's socket, reap dropped links (tearing their sessions down),
    /// parse and dispatch every complete frame, sweep expired invites, then
    /// flush outbound queues. Returns the number of events processed — 0
    /// means the relay was fully idle.
    pub fn poll_once(&mut self, now: Instant) -> io::Result<u32> {
        let mut events = 0u32;

        loop {
            match self.listener.accept() {
                Ok((sock, _addr)) => {
                    sock.set_nonblocking(true)?;
                    sock.set_nodelay(true)?;
                    let id = self.next_conn_id;
                    self.next_conn_id += 1;
                    self.conns.insert(
                        id,
                        RelayConn {
                            sock,
                            role: ConnRole::Handshake,
                            inbuf: Vec::new(),
                            outbuf: Vec::new(),
                            dead: false,
                            close_after_flush: false,
                        },
                    );
                    events += 1;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }

        let mut tmp = [0u8; 8192];
        for c in self.conns.values_mut() {
            if c.dead {
                continue;
            }
            loop {
                match c.sock.read(&mut tmp) {
                    Ok(0) => {
                        c.dead = true;
                        break;
                    }
                    Ok(n) => {
                        c.inbuf.extend_from_slice(&tmp[..n]);
                        events += 1;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        c.dead = true;
                        break;
                    }
                }
            }
        }

        self.reap_dead(&mut events);

        // Parse and dispatch every complete frame on each live conn. The
        // declared frame length is validated against the cap before the
        // body is copied out — oversized prefixes die on the header alone.
        let ids: Vec<u64> = self.conns.keys().copied().collect();
        for id in ids {
            loop {
                let body = {
                    let Some(c) = self.conns.get_mut(&id) else {
                        break;
                    };
                    if c.dead || c.close_after_flush {
                        break;
                    }
                    match next_frame_len(&c.inbuf) {
                        Err(_) => {
                            c.dead = true;
                            break;
                        }
                        Ok(None) => break,
                        Ok(Some(total)) => {
                            let body = c.inbuf[4..total].to_vec();
                            c.inbuf.drain(..total);
                            body
                        }
                    }
                };
                match decode_packet(&body) {
                    Ok(packet) => {
                        events += 1;
                        self.dispatch(id, packet, now);
                    }
                    Err(_) => self.mark_dead(id),
                }
                if self.conns.get(&id).map(|c| c.dead).unwrap_or(true) {
                    break;
                }
            }
        }

        self.reap_dead(&mut events);

        for expired in self.router.sweep_expired(now) {
            if let Some(&host_id) = self.host_conns.get(&expired.invite_code) {
                self.mark_dead(host_id);
            }
            for stream_id in expired.stream_ids {
                if let Some(guest_id) = self.stream_conns.remove(&stream_id) {
                    self.queue_packet(
                        guest_id,
                        &FriendPacket::StreamClose {
                            stream_id,
                            reason: "invite expired".to_owned(),
                        },
                    );
                    if let Some(g) = self.conns.get_mut(&guest_id) {
                        g.close_after_flush = true;
                    }
                }
            }
        }

        self.reap_dead(&mut events);

        let ids: Vec<u64> = self.conns.keys().copied().collect();
        for id in ids {
            let Some(c) = self.conns.get_mut(&id) else {
                continue;
            };
            if c.dead {
                continue;
            }
            while !c.outbuf.is_empty() {
                match c.sock.write(&c.outbuf) {
                    Ok(0) => break,
                    Ok(n) => {
                        c.outbuf.drain(..n);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        c.dead = true;
                        break;
                    }
                }
            }
            if c.close_after_flush && c.outbuf.is_empty() {
                c.dead = true;
            }
        }

        self.reap_dead(&mut events);
        Ok(events)
    }

    /// Drop every conn flagged `dead`, unwinding whatever session state it
    /// owned: a guest link frees its stream (the host hears a `StreamClose`),
    /// a host tunnel revokes its code and every guest on it is told and
    /// flushed out.
    fn reap_dead(&mut self, events: &mut u32) {
        let ids: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, c)| c.dead)
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            *events += 1;
            self.drop_conn(id);
        }
    }

    fn drop_conn(&mut self, id: u64) {
        let Some(c) = self.conns.remove(&id) else {
            return;
        };
        match c.role {
            ConnRole::Guest { stream_id } => {
                self.stream_conns.remove(&stream_id);
                if let Some(host_code) = self.router.close_stream(stream_id) {
                    if let Some(&host_id) = self.host_conns.get(&host_code) {
                        self.queue_packet(
                            host_id,
                            &FriendPacket::StreamClose {
                                stream_id,
                                reason: "guest disconnected".to_owned(),
                            },
                        );
                    }
                }
            }
            ConnRole::Host { code } => {
                self.host_conns.remove(&code);
                if let Some(stream_ids) = self.router.unregister_host(&code) {
                    for stream_id in stream_ids {
                        if let Some(guest_id) = self.stream_conns.remove(&stream_id) {
                            self.queue_packet(
                                guest_id,
                                &FriendPacket::StreamClose {
                                    stream_id,
                                    reason: "host session ended".to_owned(),
                                },
                            );
                            if let Some(g) = self.conns.get_mut(&guest_id) {
                                g.close_after_flush = true;
                            }
                        }
                    }
                }
            }
            ConnRole::Handshake => {}
        }
    }

    fn mark_dead(&mut self, id: u64) {
        if let Some(c) = self.conns.get_mut(&id) {
            c.dead = true;
        }
    }

    fn queue_packet(&mut self, id: u64, packet: &FriendPacket) {
        if let Some(c) = self.conns.get_mut(&id) {
            // Encode straight into the outbound queue: no intermediate body or
            // frame buffer, and a failed encode leaves `outbuf` untouched.
            let _ = encode_frame_into(packet, &mut c.outbuf);
        }
    }

    /// Route one decoded frame by the conn's role. Anything outside the
    /// peer's legal set — a `StreamData` on a `stream_id` the conn does not
    /// own, a relay-only packet type arriving from a peer, a second join —
    /// is an isolation violation and the link fails closed.
    fn dispatch(&mut self, id: u64, packet: FriendPacket, now: Instant) {
        let role = match self.conns.get(&id) {
            Some(c) => c.role,
            None => return,
        };
        if let FriendPacket::Heartbeat { nonce } = packet {
            self.queue_packet(id, &FriendPacket::HeartbeatAck { nonce });
            return;
        }
        match (role, packet) {
            (
                ConnRole::Handshake,
                FriendPacket::HostRegister {
                    protocol_version,
                    world_name,
                    max_guests,
                    host_secret,
                },
            ) => {
                let code = self.router.register_host(
                    now,
                    protocol_version,
                    &world_name,
                    max_guests,
                    host_secret,
                );
                self.host_conns.insert(code, id);
                if let Some(c) = self.conns.get_mut(&id) {
                    c.role = ConnRole::Host { code };
                }
                self.queue_packet(id, &FriendPacket::HostRegistered { invite_code: code });
            }
            (
                ConnRole::Handshake,
                FriendPacket::GuestJoin {
                    invite_code,
                    guest_name,
                },
            ) => match self.router.guest_join(now, &invite_code, &guest_name) {
                Ok(stream_id) => {
                    let Some(&host_id) = self.host_conns.get(&invite_code) else {
                        // The session exists but its tunnel is gone — the
                        // join cannot be served.
                        self.queue_packet(
                            id,
                            &FriendPacket::GuestRejected {
                                reason: "host unavailable".to_owned(),
                            },
                        );
                        if let Some(c) = self.conns.get_mut(&id) {
                            c.close_after_flush = true;
                        }
                        return;
                    };
                    self.stream_conns.insert(stream_id, id);
                    if let Some(c) = self.conns.get_mut(&id) {
                        c.role = ConnRole::Guest { stream_id };
                    }
                    self.queue_packet(id, &FriendPacket::GuestAccepted { stream_id });
                    self.queue_packet(
                        host_id,
                        &FriendPacket::StreamOpen {
                            stream_id,
                            guest_name,
                        },
                    );
                }
                Err(reason) => {
                    self.queue_packet(id, &FriendPacket::GuestRejected { reason });
                    if let Some(c) = self.conns.get_mut(&id) {
                        c.close_after_flush = true;
                    }
                }
            },
            (
                ConnRole::Host { code },
                FriendPacket::StreamData {
                    stream_id,
                    payload,
                },
            ) => {
                if !self.router.stream_host_ok(&code, stream_id) {
                    // A stream id outside this host's session — never
                    // deliver one host's bytes into another session.
                    self.mark_dead(id);
                    return;
                }
                match self.stream_conns.get(&stream_id).copied() {
                    Some(guest_id) => {
                        self.router.note_relayed(payload.len() as u64);
                        self.queue_packet(
                            guest_id,
                            &FriendPacket::StreamData {
                                stream_id,
                                payload,
                            },
                        );
                    }
                    None => self.mark_dead(id),
                }
            }
            (
                ConnRole::Host { code },
                FriendPacket::StreamClose { stream_id, reason },
            ) => {
                if !self.router.stream_host_ok(&code, stream_id) {
                    self.mark_dead(id);
                    return;
                }
                self.router.close_stream(stream_id);
                if let Some(guest_id) = self.stream_conns.remove(&stream_id) {
                    self.queue_packet(
                        guest_id,
                        &FriendPacket::StreamClose { stream_id, reason },
                    );
                    if let Some(g) = self.conns.get_mut(&guest_id) {
                        g.close_after_flush = true;
                    }
                }
            }
            (
                ConnRole::Host { code },
                FriendPacket::HostRevoke {
                    invite_code,
                    host_secret,
                },
            ) => {
                if invite_code != code {
                    // A tunnel may only revoke its own session.
                    self.queue_packet(id, &FriendPacket::HostRevoked { ok: false });
                    return;
                }
                match self.router.revoke(&invite_code, &host_secret) {
                    Ok(stream_ids) => {
                        self.queue_packet(id, &FriendPacket::HostRevoked { ok: true });
                        for stream_id in stream_ids {
                            if let Some(guest_id) = self.stream_conns.remove(&stream_id) {
                                self.queue_packet(
                                    guest_id,
                                    &FriendPacket::StreamClose {
                                        stream_id,
                                        reason: "invite revoked".to_owned(),
                                    },
                                );
                                if let Some(g) = self.conns.get_mut(&guest_id) {
                                    g.close_after_flush = true;
                                }
                            }
                        }
                        self.host_conns.remove(&code);
                        if let Some(c) = self.conns.get_mut(&id) {
                            c.role = ConnRole::Handshake;
                        }
                    }
                    Err(_) => self.queue_packet(id, &FriendPacket::HostRevoked { ok: false }),
                }
            }
            (
                ConnRole::Guest { stream_id: bound },
                FriendPacket::StreamData {
                    stream_id,
                    payload,
                },
            ) => {
                if stream_id != bound {
                    // Claiming another guest's stream id: the strictest
                    // fail-closed answer is the link's death — Guest A can
                    // never inject bytes into Guest B's stream.
                    self.mark_dead(id);
                    return;
                }
                let Some(stream) = self.router.stream(stream_id) else {
                    self.mark_dead(id);
                    return;
                };
                let host_code = stream.host_code;
                match self.host_conns.get(&host_code).copied() {
                    Some(host_id) => {
                        self.router.note_relayed(payload.len() as u64);
                        self.queue_packet(
                            host_id,
                            &FriendPacket::StreamData {
                                stream_id,
                                payload,
                            },
                        );
                    }
                    None => self.mark_dead(id),
                }
            }
            (
                ConnRole::Guest { stream_id: bound },
                FriendPacket::StreamClose { stream_id, .. },
            ) => {
                if stream_id != bound {
                    self.mark_dead(id);
                    return;
                }
                if let Some(host_code) = self.router.close_stream(stream_id) {
                    if let Some(&host_id) = self.host_conns.get(&host_code) {
                        self.queue_packet(
                            host_id,
                            &FriendPacket::StreamClose {
                                stream_id,
                                reason: "guest closed the stream".to_owned(),
                            },
                        );
                    }
                }
                self.mark_dead(id);
            }
            // Everything else — relay-only packet types from a peer, a
            // second HostRegister, a GuestJoin on a live stream, a
            // HostRevoke with no session — fails closed.
            _ => self.mark_dead(id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: [u8; 16] = [0x7C; 16];

    fn fixed_code(byte: u8) -> InviteCode {
        InviteCode::from_bytes([byte; 5])
    }

    fn roundtrip(packet: FriendPacket) {
        let body = encode_packet(&packet).unwrap();
        assert!(body.len() <= MAX_FRL1_FRAME_BYTES);
        assert_eq!(decode_packet(&body).unwrap(), packet);
        let frame = encode_frame(&packet).unwrap();
        assert_eq!(decode_frame(&frame).unwrap(), packet);
    }

    #[test]
    fn invite_code_formats_and_parses() {
        // All-zero and all-one corners of the 40-bit space.
        let zero = InviteCode::from_bytes([0; 5]);
        assert_eq!(zero.to_string(), "MEGU-0000-0000");
        let max = InviteCode::from_bytes([0xFF; 5]);
        assert_eq!(max.to_string(), "MEGU-ZZZZ-ZZZZ");

        // Roundtrip through the canonical display form.
        let c = fixed_code(0xAB);
        let text = c.to_string();
        assert!(text.starts_with("MEGU-"));
        assert_eq!(text.len(), 14);
        assert_eq!(InviteCode::parse(&text).unwrap(), c);

        // Tolerant forms: lowercase, no hyphens, no prefix, both.
        assert_eq!(InviteCode::parse(&text.to_lowercase()).unwrap(), c);
        assert_eq!(
            InviteCode::parse(&text.replace('-', "")).unwrap(),
            c,
            "hyphens are optional"
        );
        let bare = &text[5..];
        assert_eq!(InviteCode::parse(bare).unwrap(), c, "prefix is optional");
        assert_eq!(
            InviteCode::parse(&bare.replace('-', "")).unwrap(),
            c
        );

        // Homoglyph normalizations: o->0, i/l->1.
        assert_eq!(
            InviteCode::parse("MEGU-OOOO-OOOO").unwrap(),
            InviteCode::from_bytes([0; 5])
        );
        assert_eq!(
            InviteCode::parse("megu-ii1i-ii1i").unwrap(),
            InviteCode::parse("MEGU-1111-1111").unwrap()
        );
        assert_eq!(
            InviteCode::parse("MEGU-l0o1-L0O1").unwrap(),
            InviteCode::parse("MEGU-1001-1001").unwrap()
        );
    }

    #[test]
    fn invite_code_parse_fails_closed() {
        // Wrong digit count.
        assert!(InviteCode::parse("MEGU-XXXX-XXX").is_err());
        assert!(InviteCode::parse("MEGU-XXXX-XXXXX").is_err());
        assert!(InviteCode::parse("").is_err());
        // Character outside the alphabet (U is not a normalized homoglyph).
        assert!(InviteCode::parse("MEGU-XXXX-XXXU").is_err());
        assert!(InviteCode::parse("MEGU-****-XXXX").is_err());
        // Non-ASCII.
        assert!(InviteCode::parse("MEGU-XXÄX-XXXX").is_err());
    }

    #[test]
    fn invite_code_eq_ct_matches_equality() {
        let a = fixed_code(0x11);
        let b = fixed_code(0x11);
        let c = fixed_code(0x12);
        assert!(a.eq_ct(&b));
        assert!(!a.eq_ct(&c));
    }

    #[test]
    fn codec_roundtrips_every_packet_type() {
        roundtrip(FriendPacket::HostRegister {
            protocol_version: 767,
            world_name: "Miku's World".to_owned(),
            max_guests: 4,
            host_secret: SECRET,
        });
        roundtrip(FriendPacket::HostRegistered {
            invite_code: fixed_code(0x5E),
        });
        roundtrip(FriendPacket::HostRevoke {
            invite_code: fixed_code(0x5E),
            host_secret: SECRET,
        });
        roundtrip(FriendPacket::HostRevoked { ok: true });
        roundtrip(FriendPacket::HostRevoked { ok: false });
        roundtrip(FriendPacket::GuestJoin {
            invite_code: fixed_code(0x5E),
            guest_name: "Teto".to_owned(),
        });
        roundtrip(FriendPacket::GuestAccepted { stream_id: 7 });
        roundtrip(FriendPacket::GuestRejected {
            reason: "world is full".to_owned(),
        });
        roundtrip(FriendPacket::StreamOpen {
            stream_id: 9,
            guest_name: "Neru".to_owned(),
        });
        roundtrip(FriendPacket::StreamData {
            stream_id: 9,
            payload: b"minecraft-bytes".to_vec(),
        });
        roundtrip(FriendPacket::StreamData {
            stream_id: 9,
            payload: Vec::new(),
        });
        roundtrip(FriendPacket::StreamClose {
            stream_id: 9,
            reason: "guest quit".to_owned(),
        });
        roundtrip(FriendPacket::Heartbeat { nonce: u64::MAX });
        roundtrip(FriendPacket::HeartbeatAck { nonce: 42 });
    }

    #[test]
    fn codec_fails_closed_on_bad_input() {
        // Foreign magic, bare magic, unknown tag, oversize, truncation,
        // trailing garbage, forged string length, invalid UTF-8.
        assert!(decode_packet(b"NOPE.....").is_err());
        assert!(decode_packet(b"FRL1").is_err());
        assert!(decode_packet(b"FRL1\x99").is_err());
        assert!(decode_packet(&vec![0u8; MAX_FRL1_FRAME_BYTES + 1]).is_err());
        let frame = encode_frame(&FriendPacket::Heartbeat { nonce: 1 }).unwrap();
        assert!(decode_frame(&frame[..frame.len() - 1]).is_err());
        let mut extra = frame.clone();
        extra.push(0);
        assert!(decode_frame(&extra).is_err());
        // Oversized declared length dies on the 4-byte header alone.
        let mut fat = Vec::new();
        fat.extend_from_slice(&(MAX_FRL1_FRAME_BYTES as u32 + 1).to_be_bytes());
        assert!(decode_frame(&fat).is_err());
        // A GuestJoin whose declared string length exceeds the cap.
        let mut forged = Vec::new();
        forged.extend_from_slice(&FRIEND_MAGIC);
        forged.push(TAG_GUEST_JOIN);
        forged.extend_from_slice(&[0u8; 5]);
        forged.extend_from_slice(&0xFF_u32.to_be_bytes()[1..]); // VarInt 127
        assert!(decode_packet(&forged).is_err());
        let mut bad_utf8 = Vec::new();
        bad_utf8.extend_from_slice(&FRIEND_MAGIC);
        bad_utf8.push(TAG_STREAM_CLOSE);
        bad_utf8.extend_from_slice(&1u32.to_be_bytes());
        bad_utf8.extend_from_slice(&[3u8, 0xff, 0xfe, 0xfd]);
        assert!(matches!(
            decode_packet(&bad_utf8),
            Err(ProtoError::InvalidUtf8)
        ));
        // Encode refuses an oversized payload rather than truncating.
        assert!(encode_packet(&FriendPacket::StreamData {
            stream_id: 1,
            payload: vec![0u8; MAX_STREAM_PAYLOAD_BYTES + 1],
        })
        .is_err());
        // And an oversized string field.
        assert!(encode_packet(&FriendPacket::GuestRejected {
            reason: "x".repeat(MAX_FRL1_STRING_CHARS * 2),
        })
        .is_err());
    }

    #[test]
    fn router_admits_up_to_cap_then_rejects() {
        let mut r = FriendRelayRouter::with_seed(FriendRelayConfig::default(), 0xC0DE);
        let now = Instant::now();
        let code = r.register_host(now, 767, "cap world", 2, SECRET);
        assert_eq!(r.guest_join(now, &code, "a").unwrap(), 1);
        assert_eq!(r.guest_join(now, &code, "b").unwrap(), 2);
        assert_eq!(
            r.guest_join(now, &code, "c"),
            Err("world is full".to_owned())
        );
        assert_eq!(r.metrics().rejected_joins, 1);
        assert_eq!(r.metrics().active_guests, 2);
        // A zero-cap host admits nobody.
        let code0 = r.register_host(now, 767, "empty", 0, SECRET);
        assert!(r.guest_join(now, &code0, "a").is_err());
    }

    #[test]
    fn router_expiry_is_instant_driven() {
        let mut r = FriendRelayRouter::with_seed(
            FriendRelayConfig {
                invite_ttl: Duration::from_secs(60),
            },
            0xBEEF,
        );
        let t0 = Instant::now();
        let code = r.register_host(t0, 767, "short-lived", 2, SECRET);
        assert!(r.guest_join(t0 + Duration::from_secs(59), &code, "a").is_ok());
        let t1 = t0 + Duration::from_secs(61);
        assert_eq!(
            r.guest_join(t1, &code, "b"),
            Err("invite expired".to_owned())
        );
        // The sweep retires the session and reports the open stream.
        let expired = r.sweep_expired(t1);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].invite_code, code);
        assert_eq!(expired[0].stream_ids, vec![1]);
        assert!(r.host(&code).is_none());
        assert_eq!(r.metrics().revoked_codes, 1);
        // And afterwards everything fails closed.
        assert_eq!(
            r.guest_join(t1, &code, "c"),
            Err("unknown invite".to_owned())
        );
    }

    #[test]
    fn router_revoke_requires_the_secret() {
        let mut r = FriendRelayRouter::with_seed(FriendRelayConfig::default(), 0xF00D);
        let now = Instant::now();
        let code = r.register_host(now, 767, "revocables", 2, SECRET);
        r.guest_join(now, &code, "a").unwrap();
        r.guest_join(now, &code, "b").unwrap();

        // Wrong secret: fails closed, nothing changes.
        assert_eq!(
            r.revoke(&code, &[0xEE; 16]),
            Err(RevokeError::BadSecret)
        );
        assert_eq!(r.metrics().revoked_codes, 0);
        assert_eq!(r.metrics().active_guests, 2);
        // Unknown code fails a different way.
        assert_eq!(
            r.revoke(&fixed_code(0x99), &SECRET),
            Err(RevokeError::UnknownCode)
        );
        // Right secret: the session and both streams are gone.
        let closed = r.revoke(&code, &SECRET).unwrap();
        assert_eq!(closed.len(), 2);
        assert!(r.host(&code).is_none());
        assert_eq!(r.metrics().revoked_codes, 1);
        assert_eq!(
            r.guest_join(now, &code, "c"),
            Err("unknown invite".to_owned())
        );
    }

    #[test]
    fn router_enforces_per_host_stream_isolation() {
        let mut r = FriendRelayRouter::with_seed(FriendRelayConfig::default(), 0xACE);
        let now = Instant::now();
        let alpha = r.register_host(now, 767, "alpha", 2, SECRET);
        let beta = r.register_host(now, 767, "beta", 2, SECRET);
        let s_a1 = r.guest_join(now, &alpha, "a1").unwrap();
        let s_a2 = r.guest_join(now, &alpha, "a2").unwrap();
        let s_b1 = r.guest_join(now, &beta, "b1").unwrap();
        assert_ne!(s_a1, s_a2);
        assert_ne!(s_a1, s_b1);

        // Each stream belongs to exactly its own session.
        assert!(r.stream_host_ok(&alpha, s_a1));
        assert!(r.stream_host_ok(&alpha, s_a2));
        assert!(r.stream_host_ok(&beta, s_b1));
        assert!(!r.stream_host_ok(&alpha, s_b1));
        assert!(!r.stream_host_ok(&beta, s_a1));
        assert!(!r.stream_host_ok(&alpha, 999_999));

        // Closing a stream frees a guest slot on its own session only.
        assert_eq!(r.close_stream(s_a2), Some(alpha));
        assert_eq!(r.host(&alpha).unwrap().stream_count(), 1);
        assert!(r.stream(s_a2).is_none());
        assert!(r.guest_join(now, &alpha, "a3").is_ok());
    }

    #[test]
    fn drawn_codes_are_forty_bits_and_unique() {
        let mut r = FriendRelayRouter::with_seed(FriendRelayConfig::default(), 1);
        let now = Instant::now();
        let mut seen = BTreeSet::new();
        for _ in 0..64 {
            let c = r.register_host(now, 767, "w", 1, SECRET);
            // Roundtrip through the public parse/display surface proves the
            // stored five bytes really are the 40-bit code.
            assert_eq!(InviteCode::parse(&c.to_string()).unwrap(), c);
            seen.insert(c);
        }
        assert_eq!(seen.len(), 64, "invite codes must not collide");
    }
}
