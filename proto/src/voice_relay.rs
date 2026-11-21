// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Clean-room, zero-dependency UDP proximity voice relay (`VCR1` datagrams).
//!
//! Multiplayer voice-chat servers are, at the transport layer, a
//! high-throughput low-latency UDP relay: clients authenticate once with a
//! per-session secret, then stream opaque Opus audio frames that the server
//! fans out to the players who are allowed to hear them — same dimension,
//! within earshot, not deafened, not blocking the sender — plus group
//! channels that bypass proximity entirely. This module implements exactly
//! that relay in pure `std` (`std::net::UdpSocket`, no Netty, no GC, no
//! external crates), split into a sans-IO routing core and a thin live
//! socket wrapper so every routing rule is verifiable deterministically.
//!
//! ## `VCR1` wire shape
//!
//! Every datagram is `b"VCR1"` + a one-byte tag + big-endian fields, and the
//! whole datagram is capped at [`MAX_VOICE_DATAGRAM_BYTES`] (1400) so it
//! stays under a path MTU with headroom. Oversized, truncated, bad-magic or
//! bad-tag datagrams fail closed through [`ProtoError`]; nothing panics on
//! attacker-controlled input.
//!
//! * `0x01 Auth`: `uuid(16) secret(16)` — client binds its `SocketAddr`.
//! * `0x02 AuthAck`: `ok(1)` — relay answers whether the secret matched.
//! * `0x03 MicFrame`: `uuid(16) seq(u64) whispering(1) len(u16) opus(len)`
//!   — one upstream audio frame; `opus` is opaque to the relay.
//! * `0x04 KeepAlive`: `uuid(16) nonce(u64)`.
//! * `0x05 KeepAliveAck`: `nonce(u64)` — echoes the authenticated nonce.
//! * `0x06 PlayerSound`: `sender(16) seq(u64) whispering(1) distance_sq(f32)
//!   group(1) len(u16) opus(len)` — one downstream copy per recipient;
//!   `group` marks a group-channel delivery (distance fields then carry 0),
//!   otherwise `distance_sq` is the exact squared sender distance.
//!
//! ## Layers
//!
//! * [`VoicePacket`] + [`encode_packet`]/[`decode_packet`] — the `VCR1`
//!   codec.
//! * [`VoiceRelayState`] — the sans-IO router: player table, group kinds,
//!   and [`VoiceRelayState::route_mic`], which decides who hears a frame
//!   using only state + a caller-supplied [`Instant`] (no `SystemTime`
//!   anywhere — the rate-limit window is driven by the caller's clock).
//! * [`UdpVoiceRelay`] — the live relay: a non-blocking `UdpSocket`, one
//!   [`UdpVoiceRelay::poll_once`] drain per tick, and [`VoiceRelayMetrics`]
//!   counters for everything it accepted or rejected.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use crate::wire::{ProtoError, WireReader, WireWriter};

/// Hard cap on a `VCR1` datagram: 1400 bytes keeps a packet plus IPv4/UDP
/// headers under the Ethernet MTU with room for a tunnel. Anything larger —
/// or anything that had to be truncated to fit the receive buffer — is
/// malformed, never relayed.
pub const MAX_VOICE_DATAGRAM_BYTES: usize = 1400;

/// Magic bytes every `VCR1` datagram opens with. Foreign traffic sharing the
/// port fails closed on this check.
pub const VOICE_MAGIC: [u8; 4] = *b"VCR1";

/// Sliding window length for the per-sender MicFrame rate cap. Driven by the
/// caller-supplied [`Instant`], so tests advance it by stepping a fake or by
/// simply calling close together in real time.
pub const VOICE_RATE_WINDOW: Duration = Duration::from_secs(1);

/// Largest opaque audio payload a `MicFrame` may carry, sized so the
/// forwarded `PlayerSound` (which adds `distance_sq` + `group` = 5 bytes of
/// header over `MicFrame`) still fits the datagram cap.
pub const MAX_OPUS_PAYLOAD_BYTES: usize = MAX_VOICE_DATAGRAM_BYTES - 37;

const TAG_AUTH: u8 = 0x01;
const TAG_AUTH_ACK: u8 = 0x02;
const TAG_MIC_FRAME: u8 = 0x03;
const TAG_KEEPALIVE: u8 = 0x04;
const TAG_KEEPALIVE_ACK: u8 = 0x05;
const TAG_PLAYER_SOUND: u8 = 0x06;

/// Constant-time comparison for the 16-byte session secrets: the compare
/// always touches all 16 bytes so an attacker timing Auth replies learns
/// nothing about how many prefix bytes were right.
fn secret_eq(a: &[u8; 16], b: &[u8; 16]) -> bool {
    let mut diff = 0u8;
    for i in 0..16 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// One decoded `VCR1` datagram. `Auth`, `MicFrame` and `KeepAlive` travel
/// client → relay; `AuthAck`, `KeepAliveAck` and `PlayerSound` travel relay →
/// client. The relay treats a datagram of the wrong direction as malformed.
#[derive(Clone, Debug, PartialEq)]
pub enum VoicePacket {
    /// Client → relay: bind this socket's address to `player_uuid`, gated on
    /// the 16-byte session secret issued out-of-band (e.g. over the TCP
    /// login channel).
    Auth {
        /// Player the sender claims to be.
        player_uuid: [u8; 16],
        /// Session secret the relay compares in constant time.
        secret: [u8; 16],
    },
    /// Relay → client: result of the latest `Auth`.
    AuthAck {
        /// `true` iff the secret matched; the client may now send MicFrames.
        ok: bool,
    },
    /// Client → relay: one upstream audio frame. `opus_payload` is opaque
    /// (typically Opus, possibly already encrypted end-to-end); the relay
    /// forwards the bytes verbatim.
    MicFrame {
        /// Sender; must be authenticated and bound to the source address.
        player_uuid: [u8; 16],
        /// Sender-side sequence number, echoed to recipients for jitter
        /// buffering and gap detection.
        seq: u64,
        /// `true` restricts delivery to the whisper radius.
        whispering: bool,
        /// Opaque audio bytes, capped at [`MAX_OPUS_PAYLOAD_BYTES`].
        opus_payload: Vec<u8>,
    },
    /// Client → relay: liveness probe for the authenticated binding.
    KeepAlive {
        /// Player whose binding is being refreshed.
        player_uuid: [u8; 16],
        /// Opaque token echoed back in `KeepAliveAck`.
        nonce: u64,
    },
    /// Relay → client: liveness answer echoing the probe's nonce.
    KeepAliveAck {
        /// The nonce from the `KeepAlive` being answered.
        nonce: u64,
    },
    /// Relay → client: one downstream audio copy. `group = true` marks a
    /// group-channel delivery that bypassed proximity (distance fields are
    /// then 0); `group = false` is a proximity delivery carrying the exact
    /// squared distance the router measured.
    PlayerSound {
        /// Original sender of the frame.
        sender_uuid: [u8; 16],
        /// Sequence number copied from the `MicFrame`.
        seq: u64,
        /// Whisper flag copied from the `MicFrame`.
        whispering: bool,
        /// Squared sender→recipient distance for proximity deliveries, 0.0
        /// for group deliveries.
        distance_sq: f32,
        /// `true` = group channel, `false` = spatial proximity.
        group: bool,
        /// The verbatim audio bytes from the `MicFrame`.
        opus_payload: Vec<u8>,
    },
}

/// Encode one packet into a `VCR1` datagram. Fails closed — rather than
/// truncating — when the result would exceed [`MAX_VOICE_DATAGRAM_BYTES`]
/// or an audio payload exceeds [`MAX_OPUS_PAYLOAD_BYTES`].
pub fn encode_packet(packet: &VoicePacket) -> Result<Vec<u8>, ProtoError> {
    let mut w = WireWriter::with_capacity(64);
    w.raw(&VOICE_MAGIC);
    match packet {
        VoicePacket::Auth {
            player_uuid,
            secret,
        } => {
            w.write_u8(TAG_AUTH);
            w.write_uuid(player_uuid);
            w.write_uuid(secret);
        }
        VoicePacket::AuthAck { ok } => {
            w.write_u8(TAG_AUTH_ACK);
            w.write_bool(*ok);
        }
        VoicePacket::MicFrame {
            player_uuid,
            seq,
            whispering,
            opus_payload,
        } => {
            check_opus_len(opus_payload)?;
            w.write_u8(TAG_MIC_FRAME);
            w.write_uuid(player_uuid);
            w.write_u64(*seq);
            w.write_bool(*whispering);
            w.write_u16(opus_payload.len() as u16);
            w.raw(opus_payload);
        }
        VoicePacket::KeepAlive { player_uuid, nonce } => {
            w.write_u8(TAG_KEEPALIVE);
            w.write_uuid(player_uuid);
            w.write_u64(*nonce);
        }
        VoicePacket::KeepAliveAck { nonce } => {
            w.write_u8(TAG_KEEPALIVE_ACK);
            w.write_u64(*nonce);
        }
        VoicePacket::PlayerSound {
            sender_uuid,
            seq,
            whispering,
            distance_sq,
            group,
            opus_payload,
        } => {
            check_opus_len(opus_payload)?;
            w.write_u8(TAG_PLAYER_SOUND);
            w.write_uuid(sender_uuid);
            w.write_u64(*seq);
            w.write_bool(*whispering);
            w.write_f32(*distance_sq);
            w.write_bool(*group);
            w.write_u16(opus_payload.len() as u16);
            w.raw(opus_payload);
        }
    }
    let out = w.into_vec();
    if out.len() > MAX_VOICE_DATAGRAM_BYTES {
        return Err(ProtoError::FrameTooLarge {
            length: out.len(),
            max: MAX_VOICE_DATAGRAM_BYTES,
        });
    }
    Ok(out)
}

fn check_opus_len(payload: &[u8]) -> Result<(), ProtoError> {
    if payload.len() > MAX_OPUS_PAYLOAD_BYTES {
        return Err(ProtoError::LengthOutOfBounds {
            length: payload.len(),
            max: MAX_OPUS_PAYLOAD_BYTES,
        });
    }
    Ok(())
}

fn read_opus(r: &mut WireReader<'_>) -> Result<Vec<u8>, ProtoError> {
    let len = r.read_u16()? as usize;
    if len > MAX_OPUS_PAYLOAD_BYTES {
        return Err(ProtoError::LengthOutOfBounds {
            length: len,
            max: MAX_OPUS_PAYLOAD_BYTES,
        });
    }
    Ok(r.take(len)?.to_vec())
}

/// Decode one `VCR1` datagram. Every failure mode — oversize, truncation,
/// foreign magic, unknown tag, forged length, trailing garbage — is a
/// [`ProtoError`], never a panic.
pub fn decode_packet(datagram: &[u8]) -> Result<VoicePacket, ProtoError> {
    if datagram.len() > MAX_VOICE_DATAGRAM_BYTES {
        return Err(ProtoError::FrameTooLarge {
            length: datagram.len(),
            max: MAX_VOICE_DATAGRAM_BYTES,
        });
    }
    let mut r = WireReader::new(datagram);
    if r.take(VOICE_MAGIC.len())? != VOICE_MAGIC.as_slice() {
        return Err(ProtoError::Malformed("not a VCR1 voice datagram"));
    }
    let packet = match r.read_u8()? {
        TAG_AUTH => VoicePacket::Auth {
            player_uuid: r.read_uuid()?,
            secret: r.read_uuid()?,
        },
        TAG_AUTH_ACK => VoicePacket::AuthAck { ok: r.read_bool()? },
        TAG_MIC_FRAME => VoicePacket::MicFrame {
            player_uuid: r.read_uuid()?,
            seq: r.read_u64()?,
            whispering: r.read_bool()?,
            opus_payload: read_opus(&mut r)?,
        },
        TAG_KEEPALIVE => VoicePacket::KeepAlive {
            player_uuid: r.read_uuid()?,
            nonce: r.read_u64()?,
        },
        TAG_KEEPALIVE_ACK => VoicePacket::KeepAliveAck {
            nonce: r.read_u64()?,
        },
        TAG_PLAYER_SOUND => VoicePacket::PlayerSound {
            sender_uuid: r.read_uuid()?,
            seq: r.read_u64()?,
            whispering: r.read_bool()?,
            distance_sq: r.read_f32()?,
            group: r.read_bool()?,
            opus_payload: read_opus(&mut r)?,
        },
        other => {
            return Err(ProtoError::InvalidDiscriminant {
                value: other as i64,
                what: "VCR1 packet tag",
            })
        }
    };
    r.finish()?;
    Ok(packet)
}

/// What membership in a voice group does to routing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceGroupKind {
    /// Members hear each other on the group channel and still send/receive
    /// proximity audio with nearby non-group players.
    Normal,
    /// Members hear each other on the group channel; their proximity audio
    /// also reaches nearby non-group players (the group "leaks" outward by
    /// design).
    Open,
    /// Strict channel: members hear only each other. Their audio never
    /// reaches non-members and non-member proximity audio never reaches
    /// them — no leakage in either direction.
    Isolated,
}

/// Tunables for [`VoiceRelayState`].
#[derive(Clone, Debug, PartialEq)]
pub struct VoiceRelayConfig {
    /// Furthest earshot for normal speech, in blocks.
    pub max_distance: f64,
    /// Furthest earshot for whispered frames, in blocks.
    pub whisper_distance: f64,
    /// MicFrames a single sender may forward per [`VOICE_RATE_WINDOW`];
    /// beyond that the frame is dropped so one flooder cannot amplify UDP
    /// traffic through the fan-out.
    pub max_packets_per_window: u32,
    /// Master switch for group routing. `false` treats every player as
    /// group-less: no group deliveries, no isolation effects.
    pub allow_groups: bool,
}

impl Default for VoiceRelayConfig {
    fn default() -> Self {
        Self {
            max_distance: 48.0,
            whisper_distance: 16.0,
            max_packets_per_window: 64,
            allow_groups: true,
        }
    }
}

/// Everything the router knows about one player. The game layer owns
/// `dimension`/`pos`/`group_id`/`muted`/`deafened`/`blocked` (updated as
/// players move, join channels or change settings); the relay owns
/// `peer_addr`/`secret`/`authenticated` (set only by a successful `Auth`).
#[derive(Clone, Debug, PartialEq)]
pub struct VoicePlayerState {
    /// Canonical player identity.
    pub uuid: [u8; 16],
    /// Dimension id (`minecraft:overworld`, ...); proximity never crosses
    /// dimensions.
    pub dimension: String,
    /// Feet/camera position used for distance culling.
    pub pos: [f64; 3],
    /// Group channel the player is in, if any.
    pub group_id: Option<u64>,
    /// Muted senders produce no output at all.
    pub muted: bool,
    /// Deafened recipients receive nothing (group or proximity).
    pub deafened: bool,
    /// Recipients listed here never hear this player.
    pub blocked: BTreeSet<[u8; 16]>,
    /// UDP address bound by `Auth`; `None` until the first successful auth.
    pub peer_addr: Option<SocketAddr>,
    /// 16-byte session secret, compared in constant time.
    pub secret: [u8; 16],
    /// Set by a successful `Auth`; unauthenticated players neither send nor
    /// receive.
    pub authenticated: bool,
}

impl VoicePlayerState {
    /// A not-yet-authenticated player in the overworld at the origin.
    pub fn new(uuid: [u8; 16], secret: [u8; 16]) -> Self {
        Self {
            uuid,
            dimension: "minecraft:overworld".to_owned(),
            pos: [0.0; 3],
            group_id: None,
            muted: false,
            deafened: false,
            blocked: BTreeSet::new(),
            peer_addr: None,
            secret,
            authenticated: false,
        }
    }
}

/// Why [`VoiceRelayState::route_mic`] refused a frame. Each variant maps to
/// a clean drop — nothing is ever partially relayed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteReject {
    /// No player with the sender's uuid is registered.
    UnknownSender,
    /// The sender has not completed `Auth`.
    Unauthenticated,
    /// The sender is muted.
    Muted,
    /// The sender exceeded `max_packets_per_window` inside
    /// [`VOICE_RATE_WINDOW`].
    RateLimited,
}

/// One decided delivery of a MicFrame: send a `PlayerSound` to `addr`
/// carrying `distance_sq` and `group`.
#[derive(Clone, Debug, PartialEq)]
pub struct VoiceDelivery {
    /// Recipient player.
    pub recipient: [u8; 16],
    /// Recipient's authenticated UDP address.
    pub addr: SocketAddr,
    /// Squared sender distance for `group == false`; 0.0 for group
    /// deliveries, where spatial attenuation does not apply.
    pub distance_sq: f32,
    /// `true` when delivered over the group channel.
    pub group: bool,
}

/// The sans-IO proximity router: a player table, group kinds and per-sender
/// rate windows. Pure — no sockets, no clock of its own — so every rule is
/// exercised deterministically by feeding it state and [`Instant`]s.
pub struct VoiceRelayState {
    config: VoiceRelayConfig,
    players: BTreeMap<[u8; 16], VoicePlayerState>,
    groups: BTreeMap<u64, VoiceGroupKind>,
    windows: BTreeMap<[u8; 16], VecDeque<Instant>>,
}

impl VoiceRelayState {
    /// An empty routing table under `config`.
    pub fn new(config: VoiceRelayConfig) -> Self {
        Self {
            config,
            players: BTreeMap::new(),
            groups: BTreeMap::new(),
            windows: BTreeMap::new(),
        }
    }

    /// The live configuration.
    pub fn config(&self) -> &VoiceRelayConfig {
        &self.config
    }

    /// Insert or replace a player (the game layer pushes position/dimension
    /// updates through this or through [`Self::player_mut`]).
    pub fn upsert_player(&mut self, player: VoicePlayerState) {
        self.players.insert(player.uuid, player);
    }

    /// Remove a player and its rate window.
    pub fn remove_player(&mut self, uuid: &[u8; 16]) {
        self.players.remove(uuid);
        self.windows.remove(uuid);
    }

    /// Read one player.
    pub fn player(&self, uuid: &[u8; 16]) -> Option<&VoicePlayerState> {
        self.players.get(uuid)
    }

    /// Mutate one player (position, mute/deafen, block list).
    pub fn player_mut(&mut self, uuid: &[u8; 16]) -> Option<&mut VoicePlayerState> {
        self.players.get_mut(uuid)
    }

    /// Iterate the player table in uuid order (deterministic fan-out order).
    pub fn players(&self) -> impl Iterator<Item = &VoicePlayerState> {
        self.players.values()
    }

    /// Declare or change a group's kind. Membership is `group_id` equality;
    /// a group with no declared kind behaves as [`VoiceGroupKind::Normal`].
    pub fn set_group_kind(&mut self, group_id: u64, kind: VoiceGroupKind) {
        self.groups.insert(group_id, kind);
    }

    /// Forget a group's declared kind; any members left behind revert to
    /// `Normal` semantics.
    pub fn remove_group(&mut self, group_id: u64) {
        self.groups.remove(&group_id);
    }

    /// Put a player into a group. No-op (returns `false`) for unknown
    /// players or when `allow_groups` is off.
    pub fn join_group(&mut self, uuid: &[u8; 16], group_id: u64) -> bool {
        if !self.config.allow_groups {
            return false;
        }
        match self.players.get_mut(uuid) {
            Some(p) => {
                p.group_id = Some(group_id);
                true
            }
            None => false,
        }
    }

    /// Take a player out of its group. Returns `false` for unknown players.
    pub fn leave_group(&mut self, uuid: &[u8; 16]) -> bool {
        match self.players.get_mut(uuid) {
            Some(p) => {
                p.group_id = None;
                true
            }
            None => false,
        }
    }

    /// Attempt an `Auth`: constant-time secret comparison, and on success
    /// bind `addr` as the player's `peer_addr`. A wrong secret — or an
    /// unregistered uuid — fails closed and changes nothing.
    pub fn authenticate(&mut self, uuid: &[u8; 16], secret: &[u8; 16], addr: SocketAddr) -> bool {
        // Always run a 16-byte compare, even for unknown uuids, so the Auth
        // path does not leak "uuid registered?" through its timing.
        let expected = self
            .players
            .get(uuid)
            .map(|p| p.secret)
            .unwrap_or([0u8; 16]);
        if !secret_eq(&expected, secret) {
            return false;
        }
        match self.players.get_mut(uuid) {
            Some(p) => {
                p.authenticated = true;
                p.peer_addr = Some(addr);
                true
            }
            None => false,
        }
    }

    /// Drop a player's binding (disconnect / session rollover): the next
    /// datagram from its old address no longer counts as that player.
    pub fn deauthenticate(&mut self, uuid: &[u8; 16]) {
        if let Some(p) = self.players.get_mut(uuid) {
            p.authenticated = false;
            p.peer_addr = None;
        }
    }

    fn group_kind(&self, group_id: u64) -> VoiceGroupKind {
        self.groups
            .get(&group_id)
            .copied()
            .unwrap_or(VoiceGroupKind::Normal)
    }

    /// Sliding-window admission for one MicFrame. Only admitted frames are
    /// recorded, so a sender that simply stops flooding is heard again as
    /// soon as its window drains — while a sustained flood can never push
    /// more than `max_packets_per_window` forwards.
    fn admit_packet(&mut self, sender: &[u8; 16], now: Instant) -> bool {
        let window = self.windows.entry(*sender).or_default();
        while let Some(&oldest) = window.front() {
            if now.saturating_duration_since(oldest) >= VOICE_RATE_WINDOW {
                window.pop_front();
            } else {
                break;
            }
        }
        if window.len() >= self.config.max_packets_per_window as usize {
            return false;
        }
        window.push_back(now);
        true
    }

    /// Route one MicFrame: who receives it, over which channel, with what
    /// squared distance. The decision is pure routing state; the caller
    /// (`UdpVoiceRelay`) additionally requires the datagram's source address
    /// to match the sender's authenticated binding.
    ///
    /// Order of the rules, each failing closed:
    ///
    /// 1. unknown / unauthenticated / muted sender → [`RouteReject`];
    /// 2. sender over the per-window packet cap →
    ///    [`RouteReject::RateLimited`];
    /// 3. same-group recipients → `group = true`, regardless of distance or
    ///    dimension (still skipped when deafened or blocking the sender);
    /// 4. proximity → same dimension only, sender not in an `Isolated`
    ///    group, recipient not in an `Isolated` group, within
    ///    `whisper_distance` when whispering else `max_distance`; the
    ///    delivery carries the exact squared distance.
    pub fn route_mic(
        &mut self,
        sender_uuid: &[u8; 16],
        whispering: bool,
        now: Instant,
    ) -> Result<Vec<VoiceDelivery>, RouteReject> {
        let (s_dim, s_pos, s_group, s_isolated) = {
            let s = self
                .players
                .get(sender_uuid)
                .ok_or(RouteReject::UnknownSender)?;
            if !s.authenticated {
                return Err(RouteReject::Unauthenticated);
            }
            if s.muted {
                return Err(RouteReject::Muted);
            }
            let g = if self.config.allow_groups {
                s.group_id
            } else {
                None
            };
            let isolated = g
                .map(|id| self.group_kind(id) == VoiceGroupKind::Isolated)
                .unwrap_or(false);
            (s.dimension.clone(), s.pos, g, isolated)
        };
        if !self.admit_packet(sender_uuid, now) {
            return Err(RouteReject::RateLimited);
        }
        let limit = if whispering {
            self.config.whisper_distance
        } else {
            self.config.max_distance
        };
        let limit_sq = limit * limit;
        let mut out = Vec::new();
        for (&uuid, r) in &self.players {
            if uuid == *sender_uuid
                || !r.authenticated
                || r.deafened
                || r.blocked.contains(sender_uuid)
            {
                continue;
            }
            let Some(addr) = r.peer_addr else { continue };
            let r_group = if self.config.allow_groups {
                r.group_id
            } else {
                None
            };
            if s_group.is_some() && s_group == r_group {
                out.push(VoiceDelivery {
                    recipient: uuid,
                    addr,
                    distance_sq: 0.0,
                    group: true,
                });
                continue;
            }
            if s_isolated {
                continue;
            }
            let r_isolated = r_group
                .map(|id| self.group_kind(id) == VoiceGroupKind::Isolated)
                .unwrap_or(false);
            if r_isolated || r.dimension != s_dim {
                continue;
            }
            let dx = r.pos[0] - s_pos[0];
            let dy = r.pos[1] - s_pos[1];
            let dz = r.pos[2] - s_pos[2];
            let distance_sq = dx * dx + dy * dy + dz * dz;
            if distance_sq <= limit_sq {
                out.push(VoiceDelivery {
                    recipient: uuid,
                    addr,
                    distance_sq: distance_sq as f32,
                    group: false,
                });
            }
        }
        Ok(out)
    }
}

/// Counters for everything the live relay accepted or rejected. Read via
/// [`UdpVoiceRelay::metrics`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VoiceRelayMetrics {
    /// Datagrams received on the socket (before any validation).
    pub rx_packets: u64,
    /// Datagrams sent (AuthAcks, KeepAliveAcks, fanned-out PlayerSounds).
    pub tx_packets: u64,
    /// `Auth` datagrams whose secret matched.
    pub auth_ok: u64,
    /// `Auth` datagrams rejected (unknown uuid or wrong secret).
    pub auth_rejected: u64,
    /// Datagrams dropped before routing: oversize, truncated, bad magic,
    /// unknown tag, or a relay→client type arriving from a client.
    pub dropped_malformed: u64,
    /// MicFrames dropped by the per-sender rate window.
    pub dropped_rate_limited: u64,
}

/// The live relay: a non-blocking `UdpSocket` in front of a
/// [`VoiceRelayState`]. `poll_once` drains every pending datagram, so a
/// caller ticking once per game loop iteration keeps forwarding latency at
/// one drain (sub-millisecond on loopback).
pub struct UdpVoiceRelay {
    socket: UdpSocket,
    state: VoiceRelayState,
    metrics: VoiceRelayMetrics,
}

impl UdpVoiceRelay {
    /// Bind `bind_addr` (e.g. `127.0.0.1:0` for an ephemeral test port) and
    /// serve `state`. The socket is non-blocking: `poll_once` never waits.
    pub fn bind(bind_addr: SocketAddr, state: VoiceRelayState) -> io::Result<Self> {
        let socket = UdpSocket::bind(bind_addr)?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            state,
            metrics: VoiceRelayMetrics::default(),
        })
    }

    /// The address this relay is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// The routing state (read-only).
    pub fn state(&self) -> &VoiceRelayState {
        &self.state
    }

    /// The routing state for the game layer to update (positions,
    /// dimensions, group membership, mute/deafen, block lists).
    pub fn state_mut(&mut self) -> &mut VoiceRelayState {
        &mut self.state
    }

    /// Snapshot of the relay counters.
    pub fn metrics(&self) -> VoiceRelayMetrics {
        self.metrics
    }

    /// Drain every pending datagram once. `now` drives the rate-limit
    /// window; callers pass their game-loop instant so the relay never
    /// consults a clock of its own. Returns how many datagrams were
    /// processed (0 when the socket had nothing pending).
    pub fn poll_once(&mut self, now: Instant) -> io::Result<u32> {
        // One byte of headroom over the cap: a datagram that had to be
        // truncated to fit reports n = 1401 > 1400 and is correctly counted
        // malformed instead of being half-parsed.
        let mut buf = [0u8; MAX_VOICE_DATAGRAM_BYTES + 1];
        let mut handled = 0u32;
        loop {
            match self.socket.recv_from(&mut buf) {
                Ok((n, src)) => {
                    self.metrics.rx_packets += 1;
                    self.handle_datagram(src, &buf[..n], now);
                    handled += 1;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(handled)
    }

    fn handle_datagram(&mut self, src: SocketAddr, datagram: &[u8], now: Instant) {
        let packet = match decode_packet(datagram) {
            Ok(p) => p,
            Err(_) => {
                self.metrics.dropped_malformed += 1;
                return;
            }
        };
        match packet {
            VoicePacket::Auth {
                player_uuid,
                secret,
            } => {
                let ok = self.state.authenticate(&player_uuid, &secret, src);
                if ok {
                    self.metrics.auth_ok += 1;
                } else {
                    self.metrics.auth_rejected += 1;
                }
                self.send(src, &VoicePacket::AuthAck { ok });
            }
            VoicePacket::KeepAlive { player_uuid, nonce } => {
                if self.binding_matches(&player_uuid, src) {
                    self.send(src, &VoicePacket::KeepAliveAck { nonce });
                }
            }
            VoicePacket::MicFrame {
                player_uuid,
                seq,
                whispering,
                opus_payload,
            } => {
                // The source address must be the authenticated binding:
                // claiming another player's uuid over UDP changes nothing.
                if !self.binding_matches(&player_uuid, src) {
                    return;
                }
                match self.state.route_mic(&player_uuid, whispering, now) {
                    Ok(deliveries) => {
                        for d in deliveries {
                            self.send(
                                d.addr,
                                &VoicePacket::PlayerSound {
                                    sender_uuid: player_uuid,
                                    seq,
                                    whispering,
                                    distance_sq: d.distance_sq,
                                    group: d.group,
                                    opus_payload: opus_payload.clone(),
                                },
                            );
                        }
                    }
                    Err(RouteReject::RateLimited) => {
                        self.metrics.dropped_rate_limited += 1;
                    }
                    Err(RouteReject::UnknownSender)
                    | Err(RouteReject::Unauthenticated)
                    | Err(RouteReject::Muted) => {}
                }
            }
            // Relay→client packet types arriving from a client: fail closed.
            VoicePacket::AuthAck { .. }
            | VoicePacket::KeepAliveAck { .. }
            | VoicePacket::PlayerSound { .. } => {
                self.metrics.dropped_malformed += 1;
            }
        }
    }

    /// `true` iff `uuid` is authenticated and its bound address is exactly
    /// `src` — the anti-spoof check on every client→relay datagram.
    fn binding_matches(&self, uuid: &[u8; 16], src: SocketAddr) -> bool {
        self.state
            .player(uuid)
            .map(|p| p.authenticated && p.peer_addr == Some(src))
            .unwrap_or(false)
    }

    /// Best-effort single send: UDP gives no delivery guarantee and one
    /// unreachable recipient must not stall the rest of the fan-out, so a
    /// send error is silently absorbed — `tx_packets` only counts sends
    /// that made it to the wire.
    fn send(&mut self, dest: SocketAddr, packet: &VoicePacket) {
        if let Ok(bytes) = encode_packet(packet) {
            if self.socket.send_to(&bytes, dest).is_ok() {
                self.metrics.tx_packets += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: [u8; 16] = [0x5A; 16];

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn authed(uuid: [u8; 16], dim: &str, pos: [f64; 3], port: u16) -> VoicePlayerState {
        VoicePlayerState {
            uuid,
            dimension: dim.to_owned(),
            pos,
            peer_addr: Some(addr(port)),
            authenticated: true,
            ..VoicePlayerState::new(uuid, SECRET)
        }
    }

    fn roundtrip(packet: VoicePacket) {
        let bytes = encode_packet(&packet).unwrap();
        assert!(bytes.len() <= MAX_VOICE_DATAGRAM_BYTES);
        assert_eq!(decode_packet(&bytes).unwrap(), packet);
    }

    #[test]
    fn codec_roundtrips_every_packet_type() {
        roundtrip(VoicePacket::Auth {
            player_uuid: [1; 16],
            secret: [2; 16],
        });
        roundtrip(VoicePacket::AuthAck { ok: true });
        roundtrip(VoicePacket::AuthAck { ok: false });
        roundtrip(VoicePacket::MicFrame {
            player_uuid: [3; 16],
            seq: u64::MAX,
            whispering: true,
            opus_payload: b"opus-bytes".to_vec(),
        });
        roundtrip(VoicePacket::KeepAlive {
            player_uuid: [4; 16],
            nonce: 42,
        });
        roundtrip(VoicePacket::KeepAliveAck { nonce: 42 });
        roundtrip(VoicePacket::PlayerSound {
            sender_uuid: [5; 16],
            seq: 7,
            whispering: false,
            distance_sq: 100.0,
            group: false,
            opus_payload: vec![0xAB; MAX_OPUS_PAYLOAD_BYTES],
        });
    }

    #[test]
    fn codec_fails_closed_on_bad_input() {
        // Wrong magic, truncation, unknown tag, oversized, trailing garbage,
        // forged length — all errors, no panics.
        assert!(decode_packet(b"NOPE...............").is_err());
        assert!(decode_packet(b"VCR1").is_err());
        assert!(decode_packet(b"VCR1\x99").is_err());
        assert!(decode_packet(&vec![0u8; MAX_VOICE_DATAGRAM_BYTES + 1]).is_err());
        let mut extra = encode_packet(&VoicePacket::AuthAck { ok: true }).unwrap();
        extra.push(0);
        assert!(decode_packet(&extra).is_err());
        // A MicFrame whose declared opus length exceeds the cap / buffer.
        let mut forged = Vec::new();
        forged.extend_from_slice(&VOICE_MAGIC);
        forged.push(TAG_MIC_FRAME);
        forged.extend_from_slice(&[0u8; 16]);
        forged.extend_from_slice(&0u64.to_be_bytes());
        forged.push(0);
        forged.extend_from_slice(&u16::MAX.to_be_bytes());
        assert!(decode_packet(&forged).is_err());
        // And encode refuses what would not fit.
        assert!(encode_packet(&VoicePacket::MicFrame {
            player_uuid: [0; 16],
            seq: 0,
            whispering: false,
            opus_payload: vec![0u8; MAX_OPUS_PAYLOAD_BYTES + 1],
        })
        .is_err());
    }

    #[test]
    fn router_applies_proximity_rules() {
        let mut st = VoiceRelayState::new(VoiceRelayConfig::default());
        let alice = [0xA1; 16];
        let bob = [0xB0; 16];
        let far = [0xC0; 16];
        let nether = [0xD0; 16];
        st.upsert_player(authed(alice, "minecraft:overworld", [0.0, 64.0, 0.0], 1));
        st.upsert_player(authed(bob, "minecraft:overworld", [10.0, 64.0, 0.0], 2));
        st.upsert_player(authed(far, "minecraft:overworld", [80.0, 64.0, 0.0], 3));
        st.upsert_player(authed(nether, "minecraft:the_nether", [5.0, 64.0, 0.0], 4));
        let now = Instant::now();

        let out = st.route_mic(&alice, false, now).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].recipient, bob);
        assert!(!out[0].group);
        assert_eq!(out[0].distance_sq, 100.0);

        // Whisper only reaches inside 16m.
        st.player_mut(&bob).unwrap().pos = [25.0, 64.0, 0.0];
        assert!(st.route_mic(&alice, true, now).unwrap().is_empty());
        // ...but normal speech still does (25m < 48m).
        assert_eq!(st.route_mic(&alice, false, now).unwrap().len(), 1);
    }

    #[test]
    fn router_groups_and_isolation() {
        let mut st = VoiceRelayState::new(VoiceRelayConfig::default());
        let alice = [0xA1; 16];
        let bob = [0xB0; 16];
        let dave = [0xD0; 16];
        st.upsert_player(authed(alice, "minecraft:overworld", [0.0, 64.0, 0.0], 1));
        st.upsert_player(authed(bob, "minecraft:overworld", [10.0, 64.0, 0.0], 2));
        st.upsert_player(authed(dave, "minecraft:the_nether", [5.0, 64.0, 0.0], 3));
        st.set_group_kind(7, VoiceGroupKind::Isolated);
        assert!(st.join_group(&alice, 7));
        assert!(st.join_group(&dave, 7));
        let now = Instant::now();

        // Dave gets the group copy across dimensions; Bob gets nothing even
        // though he is 10m away — an isolated sender has no proximity leak.
        let out = st.route_mic(&alice, false, now).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].recipient, dave);
        assert!(out[0].group);

        // A normal group leaks both ways: members hear each other at any
        // range, and outsiders still hear proximity.
        st.set_group_kind(8, VoiceGroupKind::Normal);
        assert!(st.leave_group(&alice));
        assert!(st.join_group(&alice, 8));
        assert!(st.join_group(&dave, 8));
        let out = st.route_mic(&alice, false, now).unwrap();
        assert_eq!(out.len(), 2); // dave (group) + bob (proximity)
        assert!(out.iter().any(|d| d.recipient == dave && d.group));
        assert!(out.iter().any(|d| d.recipient == bob && !d.group));
    }

    #[test]
    fn router_mute_deafen_block_and_rate_limit() {
        let mut st = VoiceRelayState::new(VoiceRelayConfig {
            max_packets_per_window: 2,
            ..VoiceRelayConfig::default()
        });
        let alice = [0xA1; 16];
        let bob = [0xB0; 16];
        st.upsert_player(authed(alice, "minecraft:overworld", [0.0, 64.0, 0.0], 1));
        st.upsert_player(authed(bob, "minecraft:overworld", [10.0, 64.0, 0.0], 2));
        let now = Instant::now();

        st.player_mut(&bob).unwrap().deafened = true;
        assert!(st.route_mic(&alice, false, now).unwrap().is_empty());
        st.player_mut(&bob).unwrap().deafened = false;
        st.player_mut(&bob).unwrap().blocked.insert(alice);
        assert!(st.route_mic(&alice, false, now).unwrap().is_empty());
        st.player_mut(&bob).unwrap().blocked.remove(&alice);

        st.player_mut(&alice).unwrap().muted = true;
        assert_eq!(st.route_mic(&alice, false, now), Err(RouteReject::Muted));
        st.player_mut(&alice).unwrap().muted = false;

        // The deafened/blocked probes above already admitted two frames at
        // `now` (upstream traffic counts even when the fan-out is empty), so
        // step past the window for a clean rate-limit check: two admitted,
        // the third rejected.
        let now = now + VOICE_RATE_WINDOW + Duration::from_millis(1);
        assert_eq!(st.route_mic(&alice, false, now).unwrap().len(), 1);
        assert_eq!(st.route_mic(&alice, false, now).unwrap().len(), 1);
        assert_eq!(
            st.route_mic(&alice, false, now),
            Err(RouteReject::RateLimited)
        );
        // After the window drains the sender is heard again.
        assert_eq!(
            st.route_mic(
                &alice,
                false,
                now + VOICE_RATE_WINDOW + Duration::from_millis(1)
            )
            .unwrap()
            .len(),
            1
        );
    }

    #[test]
    fn unauthenticated_sender_is_rejected() {
        let mut st = VoiceRelayState::new(VoiceRelayConfig::default());
        let alice = [0xA1; 16];
        st.upsert_player(VoicePlayerState::new(alice, SECRET));
        assert_eq!(
            st.route_mic(&alice, false, Instant::now()),
            Err(RouteReject::Unauthenticated)
        );
        assert_eq!(
            st.route_mic(&[0xEE; 16], false, Instant::now()),
            Err(RouteReject::UnknownSender)
        );
        // Wrong secret binds nothing; right secret binds the address.
        assert!(!st.authenticate(&alice, &[0x00; 16], addr(9)));
        assert!(st.authenticate(&alice, &SECRET, addr(9)));
        assert_eq!(st.player(&alice).unwrap().peer_addr, Some(addr(9)));
    }
}
