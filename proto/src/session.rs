// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Dual-protocol server connection session state machine.
//!
//! [`ServerSession`] is a pure `std`, sans-IO model of the server side of a
//! Minecraft Java connection: feed it inbound frames (`VarInt(len)` +
//! `VarInt(id)` + payload) through [`ServerSession::on_frame`] and it answers
//! with outbound frame bytes plus structured [`SessionEvent`]s. No sockets,
//! no clocks, no allocation beyond the packets themselves — so the same state
//! machine embeds in a blocking TCP listener, an async runtime, or a loopback
//! test driving a `1.21.1`/`767` client and a `26.3`/`777` client through one
//! [`VersionRouter`].
//!
//! The flow it implements, per connection:
//!
//! ```text
//! Handshake ──next_state=1──> Status   (status_request -> status_response,
//!     │                               ping_request  -> pong_response)
//!     └──next_state=2──> Login         (login_start -> accept_login/
//!         │                           reject_login; optional set_compression,
//!         │                           login_success; login_acknowledged)
//!         └──> Configuration           (registry_data…, finish_configuration)
//!             └──> Play                (keep_alive, block_update, open_screen,
//!                                     ticking_state, player_input, disconnect…)
//! ```
//!
//! Version negotiation happens at the handshake: the requested protocol
//! number resolves through [`VersionRouter::select`], and every later packet
//! id or wire shape flows through the selected codec's table — `keep_alive`
//! leaves as `0x26` for 767 and `0x2d` for 777, `player_input` decodes as two
//! floats + flag byte for 767 and a single `Input` flag byte for 777.
//!
//! Fail-closed rules, mirroring the rest of the crate:
//!
//! * Frames arriving in the wrong phase, unknown packet ids, malformed
//!   bodies, and anything after [`SessionPhase::Closed`] return
//!   [`ProtoError`] — nothing panics on attacker-controlled input.
//! * A *login* attempt on an unregistered protocol is rejected with
//!   `login_disconnect` naming the supported versions, then the session
//!   closes. There is no safe codec fallback for play.
//! * A *status* ping on an unregistered protocol is answered advertising the
//!   default `1.21.1`/`767` — unless
//!   [`ServerSessionConfig::strict_version_negotiation`] is set, in which
//!   case negotiation fails closed with
//!   [`ProtoError::UnsupportedVersion`] like a login attempt.
//! * Wire-valid packets the canonical IR does not model (configuration
//!   `client_information`, play `accept_teleportation`, …) are not errors —
//!   they surface as [`SessionEvent::Unhandled`] with their resolved wire
//!   name for the embedding server to handle or ignore.

use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;

use crate::codec::{self, C2SPacket, LoginProperty, ProtocolCodec, RegistryDataEntry, S2CPacket};
use crate::router::VersionRouter;
use crate::table::{PacketDir, PacketPhase};
use crate::version::{ProtocolVersion, RecipeSyncMode};
use crate::wire::{read_frame, write_frame, ProtoError, WireReader, WireWriter};

/// Invariant vanilla packet ids that do not need a negotiated table: the
/// handshake is what version negotiation happens *through*, so its id must be
/// fixed before a codec can be selected, and `login_disconnect` is what a
/// failed negotiation is reported *with*.
const ID_HANDSHAKE: i32 = 0x00;
const ID_LOGIN_DISCONNECT: i32 = 0x00;
/// Fallback status-phase ids for a ping from an unregistered protocol (the
/// `strict_version_negotiation: false` path) — the same numeric values both
/// builtin tables assign.
const ID_STATUS_RESPONSE: i32 = 0x00;
const ID_PONG_RESPONSE: i32 = 0x01;

// ---------------------------------------------------------------------------
// Public model
// ---------------------------------------------------------------------------

/// The high-level state a [`ServerSession`] is in.
///
/// `Login` carries the pending login identity plus the sub-state flag:
/// `awaiting_ack` is `false` between `login_start` and `accept_login` (the
/// embedding server is deciding) and `true` between `login_success` and the
/// client's `login_acknowledged`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionPhase {
    /// Waiting for the client's `handshake`.
    Handshake,
    /// Handshake selected the status flow; answering list-ping requests.
    Status,
    /// Login flow. `username`/`uuid` are the identity `login_start` carried
    /// (then the accepted identity once `awaiting_ack` is set).
    Login {
        username: String,
        uuid: [u8; 16],
        awaiting_ack: bool,
    },
    /// `login_acknowledged` received; registry sync and `finish_configuration`.
    Configuration,
    /// `finish_configuration` received; in-world play.
    Play,
    /// Terminal state — the transport should be dropped. Further frames fail.
    Closed,
}

/// Static knobs a [`ServerSession`] is built with.
#[derive(Clone)]
pub struct ServerSessionConfig {
    /// Codec registry consulted at the handshake to pick the wire version.
    pub router: Arc<VersionRouter>,
    /// `description.text` of the status response.
    pub motd: String,
    /// `players.max` of the status response.
    pub max_players: u32,
    /// When `Some(t)`, `accept_login` emits `set_compression` with threshold
    /// `t` ahead of `login_success`.
    pub compression_threshold: Option<i32>,
    /// When `true`, a handshake naming a protocol with no registered codec
    /// fails closed ([`ProtoError::UnsupportedVersion`], session -> `Closed`)
    /// even for a status ping. When `false`, such a ping is still answered
    /// and advertises the default `1.21.1`/`767`. A login attempt on an
    /// unregistered protocol is rejected with `login_disconnect` either
    /// way — there is no safe codec fallback for actual play.
    pub strict_version_negotiation: bool,
}

/// Something the embedding server should know about. Produced by
/// [`ServerSession::on_frame`] alongside any [`SessionOutput::SendFrame`]s.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionEvent {
    /// The handshake decoded and the protocol number routed. `matched` is
    /// `false` only on the non-strict status path for an unregistered
    /// protocol — the session then has no codec and answers status with
    /// invariant ids only.
    Negotiated {
        protocol_version: i32,
        next_state: i32,
        matched: bool,
        /// `server_address`/`server_port` verbatim from the handshake (the
        /// fields SRV redirects and proxy forwarding key on).
        server_address: String,
        server_port: u16,
    },
    /// A `status_request` was answered (the response is the matching
    /// [`SessionOutput::SendFrame`]).
    StatusRequested,
    /// A `ping_request` was echoed (the pong is the matching
    /// [`SessionOutput::SendFrame`]).
    PingResponded { nonce: i64 },
    /// `login_start` decoded — the embedding server must now call
    /// [`ServerSession::accept_login`] or [`ServerSession::reject_login`].
    LoginRequested {
        username: String,
        uuid: [u8; 16],
        /// The negotiated protocol number.
        protocol: i32,
    },
    /// `login_acknowledged` received — phase moved `Login` -> `Configuration`.
    EnteredConfiguration,
    /// `finish_configuration` received — phase moved `Configuration` -> `Play`.
    EnteredPlay,
    /// A decoded serverbound packet the canonical IR models (play-phase
    /// `keep_alive`/`player_input`/`move_vehicle`/`paddle_boat`, the
    /// configuration `keep_alive`). Variants carry their phase
    /// (`ConfigKeepAlive` vs `KeepAlive`), and `PlayerInput` vs
    /// `PlayerInputFlags` reflects the negotiated wire shape.
    Packet(C2SPacket),
    /// A wire-valid packet the canonical IR does not model, forwarded with
    /// its resolved name so the embedding server can handle or ignore it
    /// (e.g. configuration `client_information`, play `accept_teleportation`).
    Unhandled {
        phase: PacketPhase,
        name: String,
        body: Vec<u8>,
    },
    /// The peer's login handshake named a protocol with no registered codec:
    /// the `login_disconnect` reason frame is the matching
    /// [`SessionOutput::SendFrame`] and the session is now `Closed`.
    UnsupportedVersion { requested: i32, supported: Vec<i32> },
}

/// One unit of [`ServerSession::on_frame`] output: bytes to write to the
/// transport, or an event for the embedding server.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionOutput {
    /// A complete `VarInt(len) + VarInt(id) + payload` frame to send.
    SendFrame(Vec<u8>),
    /// A high-level notification.
    Event(SessionEvent),
}

impl SessionOutput {
    /// The frame bytes when this is a [`SessionOutput::SendFrame`].
    pub fn as_frame(&self) -> Option<&[u8]> {
        match self {
            Self::SendFrame(f) => Some(f),
            _ => None,
        }
    }

    /// The event when this is a [`SessionOutput::Event`].
    pub fn as_event(&self) -> Option<&SessionEvent> {
        match self {
            Self::Event(e) => Some(e),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// ServerSession
// ---------------------------------------------------------------------------

/// One server-side connection, driven by inbound frames.
///
/// Build with [`ServerSessionConfig`], then for each inbound frame call
/// [`ServerSession::on_frame`] and write every [`SessionOutput::SendFrame`]
/// it returns to the transport in order. App-driven sends
/// (`accept_login`, `send_registry_data`, `finish_configuration`,
/// `send_play`, `disconnect`) return the frame bytes directly.
pub struct ServerSession {
    config: ServerSessionConfig,
    phase: SessionPhase,
    /// The negotiated codec — `None` before the handshake and on the
    /// non-strict status path for an unregistered protocol.
    codec: Option<Arc<dyn ProtocolCodec>>,
    /// The protocol number the peer's handshake requested.
    protocol: Option<i32>,
    /// Whether `login_start` has arrived in the current `Login` phase —
    /// tracked separately from the phase payload so an empty username still
    /// counts as received.
    login_started: bool,
}

impl ServerSession {
    pub fn new(config: ServerSessionConfig) -> Self {
        Self {
            config,
            phase: SessionPhase::Handshake,
            codec: None,
            protocol: None,
            login_started: false,
        }
    }

    /// Current high-level state.
    pub fn phase(&self) -> &SessionPhase {
        &self.phase
    }

    /// The protocol number the peer's handshake requested, once routed.
    pub fn protocol_version(&self) -> Option<i32> {
        self.protocol
    }

    /// The negotiated codec, once the handshake matched a registered
    /// protocol (`None` forever on the non-strict unknown-protocol status
    /// path).
    pub fn codec(&self) -> Option<&Arc<dyn ProtocolCodec>> {
        self.codec.as_ref()
    }

    pub fn config(&self) -> &ServerSessionConfig {
        &self.config
    }

    pub fn is_closed(&self) -> bool {
        matches!(self.phase, SessionPhase::Closed)
    }

    /// Move to `Closed` without emitting anything further — the embedding
    /// server drops the transport after this.
    pub fn close(&mut self) {
        self.phase = SessionPhase::Closed;
    }

    // -- inbound ---------------------------------------------------------

    /// Consume one complete inbound frame and produce the outbound frames
    /// and events it triggers.
    pub fn on_frame(&mut self, frame: &[u8]) -> Result<Vec<SessionOutput>, ProtoError> {
        let mut out = Vec::new();
        match self.phase {
            SessionPhase::Handshake => self.on_handshake(frame, &mut out)?,
            SessionPhase::Status => self.on_status(frame, &mut out)?,
            SessionPhase::Login { .. } => self.on_login(frame, &mut out)?,
            SessionPhase::Configuration => self.on_configuration(frame, &mut out)?,
            SessionPhase::Play => self.on_play(frame, &mut out)?,
            SessionPhase::Closed => {
                return Err(ProtoError::Malformed("frame on a closed session"));
            }
        }
        Ok(out)
    }

    /// Handshake phase: the only packet legal here is `handshake` itself.
    /// Its body shape is version-invariant (that is what negotiation happens
    /// through), so it is decoded literally rather than through a codec that
    /// does not exist yet.
    fn on_handshake(
        &mut self,
        frame: &[u8],
        out: &mut Vec<SessionOutput>,
    ) -> Result<(), ProtoError> {
        let (id, body) = read_frame(frame)?;
        if id != ID_HANDSHAKE {
            return Err(ProtoError::UnknownPacketId {
                id,
                context: "handshake-phase frame",
            });
        }
        let mut r = WireReader::new(body);
        let protocol_version = r.read_varint()?;
        let server_address = r.read_string(255)?;
        let server_port = r.read_u16()?;
        let next_state = r.read_varint()?;
        r.finish()?;
        if !matches!(next_state, 1 | 2) {
            return Err(ProtoError::InvalidDiscriminant {
                value: i64::from(next_state),
                what: "handshake next state",
            });
        }
        self.protocol = Some(protocol_version);

        match self.config.router.select(protocol_version) {
            Ok(codec) => {
                self.codec = Some(codec);
                out.push(SessionOutput::Event(SessionEvent::Negotiated {
                    protocol_version,
                    next_state,
                    matched: true,
                    server_address,
                    server_port,
                }));
                self.phase = if next_state == 1 {
                    SessionPhase::Status
                } else {
                    self.login_started = false;
                    SessionPhase::Login {
                        username: String::new(),
                        uuid: [0; 16],
                        awaiting_ack: false,
                    }
                };
            }
            Err(err) => {
                if next_state == 1 {
                    if self.config.strict_version_negotiation {
                        self.phase = SessionPhase::Closed;
                        return Err(err);
                    }
                    // Status has no per-version wire shape beyond the
                    // (invariant) packet ids, so answering is safe; the JSON
                    // advertises the default version.
                    out.push(SessionOutput::Event(SessionEvent::Negotiated {
                        protocol_version,
                        next_state,
                        matched: false,
                        server_address,
                        server_port,
                    }));
                    self.phase = SessionPhase::Status;
                } else {
                    let ProtoError::UnsupportedVersion {
                        requested,
                        supported,
                    } = err
                    else {
                        // `select` only fails this way; keep the arm total.
                        return Err(err);
                    };
                    out.push(SessionOutput::SendFrame(login_disconnect_frame(
                        &unsupported_reason(requested, &supported),
                    )));
                    out.push(SessionOutput::Event(SessionEvent::UnsupportedVersion {
                        requested,
                        supported,
                    }));
                    self.phase = SessionPhase::Closed;
                }
            }
        }
        Ok(())
    }

    /// Status phase: `status_request` (empty body) -> JSON `status_response`;
    /// `ping_request` (i64) -> `pong_response` echoing it verbatim.
    fn on_status(&mut self, frame: &[u8], out: &mut Vec<SessionOutput>) -> Result<(), ProtoError> {
        let (id, body) = read_frame(frame)?;
        let name = match &self.codec {
            Some(codec) => {
                codec
                    .table()
                    .packet_name(PacketPhase::Status, PacketDir::Serverbound, id as u32)
            }
            // No codec on the non-strict unmatched path: the two status
            // serverbound ids are invariant across every version.
            None => match id {
                0x00 => Some("status_request"),
                0x01 => Some("ping_request"),
                _ => None,
            },
        }
        .ok_or(ProtoError::UnknownPacketId {
            id,
            context: "status-phase frame",
        })?;
        match name {
            "status_request" => {
                if !body.is_empty() {
                    return Err(ProtoError::Malformed("status_request carries a body"));
                }
                out.push(SessionOutput::SendFrame(self.status_response_frame()?));
                out.push(SessionOutput::Event(SessionEvent::StatusRequested));
            }
            "ping_request" => {
                let mut r = WireReader::new(body);
                let nonce = r.read_i64()?;
                r.finish()?;
                out.push(SessionOutput::SendFrame(self.status_frame(
                    "pong_response",
                    ID_PONG_RESPONSE,
                    &nonce.to_be_bytes(),
                )?));
                out.push(SessionOutput::Event(SessionEvent::PingResponded { nonce }));
            }
            _ => {
                out.push(SessionOutput::Event(SessionEvent::Unhandled {
                    phase: PacketPhase::Status,
                    name: name.to_string(),
                    body: body.to_vec(),
                }));
            }
        }
        Ok(())
    }

    /// Login phase: `login_start` surfaces as [`SessionEvent::LoginRequested`]
    /// (the app then calls `accept_login`/`reject_login`);
    /// `login_acknowledged` is legal only after `login_success` went out and
    /// moves the session to `Configuration`.
    fn on_login(&mut self, frame: &[u8], out: &mut Vec<SessionOutput>) -> Result<(), ProtoError> {
        let codec = self
            .codec
            .clone()
            .ok_or(ProtoError::Malformed("login phase without a codec"))?;
        let (id, body) = read_frame(frame)?;
        let name = codec
            .table()
            .packet_name(PacketPhase::Login, PacketDir::Serverbound, id as u32)
            .ok_or(ProtoError::UnknownPacketId {
                id,
                context: "login-phase frame",
            })?
            .to_owned();
        match name.as_str() {
            "login_start" => {
                if self.login_started {
                    return Err(ProtoError::Malformed("duplicate login_start"));
                }
                let packet = codec::decode_c2s_in(
                    codec.table(),
                    codec.capabilities(),
                    PacketPhase::Login,
                    frame,
                )?;
                let C2SPacket::LoginStart { name, uuid } = packet else {
                    return Err(ProtoError::Malformed(
                        "login_start decoded to a different packet",
                    ));
                };
                self.login_started = true;
                self.phase = SessionPhase::Login {
                    username: name.clone(),
                    uuid,
                    awaiting_ack: false,
                };
                out.push(SessionOutput::Event(SessionEvent::LoginRequested {
                    username: name,
                    uuid,
                    protocol: self.protocol.unwrap_or_default(),
                }));
            }
            "login_acknowledged" => {
                if !matches!(
                    self.phase,
                    SessionPhase::Login {
                        awaiting_ack: true,
                        ..
                    }
                ) {
                    return Err(ProtoError::Malformed(
                        "login_acknowledged before login_success",
                    ));
                }
                // Decode through the codec so a non-empty body fails closed.
                codec::decode_c2s_in(
                    codec.table(),
                    codec.capabilities(),
                    PacketPhase::Login,
                    frame,
                )?;
                self.phase = SessionPhase::Configuration;
                out.push(SessionOutput::Event(SessionEvent::EnteredConfiguration));
            }
            _ => {
                out.push(SessionOutput::Event(SessionEvent::Unhandled {
                    phase: PacketPhase::Login,
                    name,
                    body: body.to_vec(),
                }));
            }
        }
        Ok(())
    }

    /// Configuration phase: `finish_configuration` (empty body) moves the
    /// session to `Play`; `keep_alive` decodes into the IR; every other
    /// known packet (`client_information`, `select_known_packs`, …) is
    /// forwarded unmodelled.
    fn on_configuration(
        &mut self,
        frame: &[u8],
        out: &mut Vec<SessionOutput>,
    ) -> Result<(), ProtoError> {
        let codec = self
            .codec
            .clone()
            .ok_or(ProtoError::Malformed("configuration phase without a codec"))?;
        let (id, body) = read_frame(frame)?;
        let name = codec
            .table()
            .packet_name(
                PacketPhase::Configuration,
                PacketDir::Serverbound,
                id as u32,
            )
            .ok_or(ProtoError::UnknownPacketId {
                id,
                context: "configuration-phase frame",
            })?
            .to_owned();
        match name.as_str() {
            "finish_configuration" => {
                if !body.is_empty() {
                    return Err(ProtoError::Malformed("finish_configuration carries a body"));
                }
                self.phase = SessionPhase::Play;
                out.push(SessionOutput::Event(SessionEvent::EnteredPlay));
            }
            "keep_alive" => {
                let packet = codec::decode_c2s_in(
                    codec.table(),
                    codec.capabilities(),
                    PacketPhase::Configuration,
                    frame,
                )?;
                out.push(SessionOutput::Event(SessionEvent::Packet(packet)));
            }
            _ => {
                out.push(SessionOutput::Event(SessionEvent::Unhandled {
                    phase: PacketPhase::Configuration,
                    name,
                    body: body.to_vec(),
                }));
            }
        }
        Ok(())
    }

    /// Play phase: the packets the canonical IR models decode into
    /// [`SessionEvent::Packet`] through the negotiated codec (so 767 reads
    /// the float `player_input` and 777 the `Input` flag byte); every other
    /// known packet is forwarded unmodelled.
    fn on_play(&mut self, frame: &[u8], out: &mut Vec<SessionOutput>) -> Result<(), ProtoError> {
        let codec = self
            .codec
            .clone()
            .ok_or(ProtoError::Malformed("play phase without a codec"))?;
        let (id, body) = read_frame(frame)?;
        let name = codec
            .table()
            .packet_name(PacketPhase::Play, PacketDir::Serverbound, id as u32)
            .ok_or(ProtoError::UnknownPacketId {
                id,
                context: "play-phase frame",
            })?
            .to_owned();
        match name.as_str() {
            "keep_alive" | "move_vehicle" | "paddle_boat" | "player_input" => {
                let packet = codec::decode_c2s_in(
                    codec.table(),
                    codec.capabilities(),
                    PacketPhase::Play,
                    frame,
                )?;
                out.push(SessionOutput::Event(SessionEvent::Packet(packet)));
            }
            _ => {
                out.push(SessionOutput::Event(SessionEvent::Unhandled {
                    phase: PacketPhase::Play,
                    name,
                    body: body.to_vec(),
                }));
            }
        }
        Ok(())
    }

    // -- login decision --------------------------------------------------

    /// Accept the pending `login_start`: emits `set_compression` first when
    /// [`ServerSessionConfig::compression_threshold`] is set, then
    /// `login_success` in the negotiated version's wire shape — the 26.x
    /// profile appends a session-id UUID after the Game Profile properties
    /// (generated via std-only entropy here; use
    /// [`ServerSession::accept_login_with_session`] to pin it).
    ///
    /// Returns the frames to write, in order. Only legal while a login
    /// request is pending (`Login { awaiting_ack: false }` after
    /// `login_start`).
    pub fn accept_login(
        &mut self,
        uuid: [u8; 16],
        username: &str,
        properties: Vec<LoginProperty>,
    ) -> Result<Vec<Vec<u8>>, ProtoError> {
        self.accept_login_with_session(uuid, username, properties, random_session_id())
    }

    /// [`ServerSession::accept_login`] with an explicit session id — the
    /// deterministic entry point tests and session-affinity systems want.
    pub fn accept_login_with_session(
        &mut self,
        uuid: [u8; 16],
        username: &str,
        properties: Vec<LoginProperty>,
        session_id: [u8; 16],
    ) -> Result<Vec<Vec<u8>>, ProtoError> {
        if !matches!(
            self.phase,
            SessionPhase::Login {
                awaiting_ack: false,
                ..
            }
        ) || !self.login_started
        {
            return Err(ProtoError::Malformed(
                "accept_login without a pending login request",
            ));
        }
        let codec = self
            .codec
            .clone()
            .ok_or(ProtoError::Malformed("login phase without a codec"))?;
        let mut frames = Vec::new();
        if let Some(threshold) = self.config.compression_threshold {
            let mut frame = Vec::new();
            codec.encode_s2c(&S2CPacket::SetCompression { threshold }, &mut frame)?;
            frames.push(frame);
        }
        let mut frame = Vec::new();
        codec::with_body_scratch(|body| {
            let (phase, name) = codec::encode_s2c_body_into(
                codec.capabilities(),
                codec.table(),
                &S2CPacket::LoginSuccess {
                    uuid,
                    username: username.to_string(),
                    properties,
                },
                body,
            )?;
            if matches!(
                codec.capabilities().recipe_sync_mode,
                RecipeSyncMode::RecipeBookDisplays
            ) {
                body.extend_from_slice(&session_id);
            }
            codec::framed(
                codec.table(),
                phase,
                PacketDir::Clientbound,
                name,
                body,
                &mut frame,
            )
        })?;
        frames.push(frame);
        self.phase = SessionPhase::Login {
            username: username.to_string(),
            uuid,
            awaiting_ack: true,
        };
        Ok(frames)
    }

    /// Reject the pending login with `login_disconnect` (`reason` is an
    /// anonymous-NBT component span — see [`text_component`]) and move to
    /// `Closed`. Legal anywhere in the `Login` phase.
    pub fn reject_login(&mut self, reason: &[u8]) -> Result<Vec<u8>, ProtoError> {
        if !matches!(self.phase, SessionPhase::Login { .. }) {
            return Err(ProtoError::Malformed(
                "reject_login outside the login phase",
            ));
        }
        let codec = self
            .codec
            .clone()
            .ok_or(ProtoError::Malformed("login phase without a codec"))?;
        let mut frame = Vec::new();
        codec.encode_s2c(
            &S2CPacket::LoginDisconnect {
                reason: reason.to_vec(),
            },
            &mut frame,
        )?;
        self.phase = SessionPhase::Closed;
        Ok(frame)
    }

    // -- app-driven sends -------------------------------------------------

    /// Emit a `registry_data` frame. Legal only in `Configuration`.
    pub fn send_registry_data(
        &mut self,
        registry_id: &str,
        entries: Vec<RegistryDataEntry>,
    ) -> Result<Vec<u8>, ProtoError> {
        if !matches!(self.phase, SessionPhase::Configuration) {
            return Err(ProtoError::Malformed(
                "send_registry_data outside the configuration phase",
            ));
        }
        self.encode_s2c_checked(
            &S2CPacket::ConfigRegistryData {
                registry_id: registry_id.to_string(),
                entries,
            },
            PacketPhase::Configuration,
        )
    }

    /// Emit the `finish_configuration` frame under the active codec's table
    /// id. Legal only in `Configuration`; the session still advances to
    /// `Play` only when the client's own `finish_configuration` arrives.
    pub fn finish_configuration(&mut self) -> Result<Vec<u8>, ProtoError> {
        if !matches!(self.phase, SessionPhase::Configuration) {
            return Err(ProtoError::Malformed(
                "finish_configuration outside the configuration phase",
            ));
        }
        self.encode_s2c_checked(
            &S2CPacket::ConfigFinishConfiguration,
            PacketPhase::Configuration,
        )
    }

    /// Emit one play-phase S2C packet (`KeepAlive`, `BlockUpdate`,
    /// `ForgetLevelChunk`, `TickingState`, `TickingStep`, `SetPlayerTeam`,
    /// `SetEquipment`, `SetPassengers`, `OpenScreen`, `TabList`,
    /// `PlayDisconnect`, …) through the negotiated codec — ids resolve
    /// through the active table, so the same IR leaves as `0x26` for a 767
    /// client and `0x2d` for a 777 one. Legal only in `Play`; a non-play IR
    /// variant fails closed rather than leaking a login-phase frame onto a
    /// playing connection.
    pub fn send_play(&mut self, packet: &S2CPacket) -> Result<Vec<u8>, ProtoError> {
        if !matches!(self.phase, SessionPhase::Play) {
            return Err(ProtoError::Malformed("send_play outside the play phase"));
        }
        self.encode_s2c_checked(packet, PacketPhase::Play)
    }

    /// Emit the phase-appropriate `disconnect` packet (`login_disconnect` in
    /// `Login`, `disconnect` in `Configuration`/`Play`) and move to `Closed`.
    pub fn disconnect(&mut self, reason: &[u8]) -> Result<Vec<u8>, ProtoError> {
        let (packet, want) = match self.phase {
            SessionPhase::Login { .. } => (
                S2CPacket::LoginDisconnect {
                    reason: reason.to_vec(),
                },
                PacketPhase::Login,
            ),
            SessionPhase::Configuration => (
                S2CPacket::ConfigDisconnect {
                    reason: reason.to_vec(),
                },
                PacketPhase::Configuration,
            ),
            SessionPhase::Play => (
                S2CPacket::PlayDisconnect {
                    reason: reason.to_vec(),
                },
                PacketPhase::Play,
            ),
            _ => {
                return Err(ProtoError::Malformed(
                    "disconnect has no wire form in this phase",
                ))
            }
        };
        let frame = self.encode_s2c_checked(&packet, want)?;
        self.phase = SessionPhase::Closed;
        Ok(frame)
    }

    // -- internals ---------------------------------------------------------

    /// Encode `packet` through the negotiated codec, requiring it to belong
    /// to wire phase `want` — the gate that keeps a login-phase IR variant
    /// off a play-phase connection.
    fn encode_s2c_checked(
        &self,
        packet: &S2CPacket,
        want: PacketPhase,
    ) -> Result<Vec<u8>, ProtoError> {
        let codec = self
            .codec
            .as_ref()
            .ok_or(ProtoError::Malformed("no negotiated codec"))?;
        let mut out = Vec::new();
        codec::with_body_scratch(|body| {
            let (phase, name) =
                codec::encode_s2c_body_into(codec.capabilities(), codec.table(), packet, body)?;
            if phase != want {
                return Err(ProtoError::Malformed(
                    "packet does not belong to the session phase",
                ));
            }
            codec::framed(
                codec.table(),
                phase,
                PacketDir::Clientbound,
                name,
                body,
                &mut out,
            )
        })?;
        Ok(out)
    }

    /// One status clientbound frame, resolving the id through the negotiated
    /// table when a codec exists and falling back to the invariant id when
    /// the ping came from an unregistered protocol.
    fn status_frame(
        &self,
        name: &'static str,
        fallback_id: i32,
        body: &[u8],
    ) -> Result<Vec<u8>, ProtoError> {
        let id = match &self.codec {
            Some(codec) => codec
                .table()
                .packet_id(PacketPhase::Status, PacketDir::Clientbound, name)
                .map(|v| v as i32)
                .ok_or(ProtoError::UnknownPacketId {
                    id: -1,
                    context: "status s2c table lookup",
                })?,
            None => fallback_id,
        };
        let mut out = Vec::new();
        write_frame(&mut out, id, body);
        Ok(out)
    }

    /// The `status_response` JSON: matched codec's version, or the default
    /// `1.21.1`/`767` advertisement for an unregistered ping.
    fn status_response_frame(&self) -> Result<Vec<u8>, ProtoError> {
        let (name, protocol) = match &self.codec {
            Some(codec) => {
                let caps = codec.capabilities();
                (caps.mc_version, caps.protocol_version)
            }
            None => (
                ProtocolVersion::V1_21_1.capabilities().mc_version,
                ProtocolVersion::V1_21_1_NUMBER,
            ),
        };
        let json = format!(
            "{{\"version\":{{\"name\":\"{}\",\"protocol\":{}}},\
             \"players\":{{\"max\":{},\"online\":0,\"sample\":[]}},\
             \"description\":{{\"text\":\"{}\"}}}}",
            json_escape(name),
            protocol,
            self.config.max_players,
            json_escape(&self.config.motd),
        );
        let mut w = WireWriter::new();
        w.write_string(&json);
        self.status_frame("status_response", ID_STATUS_RESPONSE, w.as_slice())
    }
}

// ---------------------------------------------------------------------------
// Shared play packet broadcast
// ---------------------------------------------------------------------------

/// Wire variant used by a broadcast recipient. A threshold of `-1` means
/// compression is disabled (`ServerSessionConfig::compression_threshold = None`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BroadcastVariantKey {
    pub protocol_version: i32,
    pub compression_threshold: i32,
}

/// Encoding and delivery counts for one logical packet broadcast.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BroadcastBatchStats {
    pub unique_encodes: usize,
    pub fanout_deliveries: usize,
}

/// A complete framed play packet whose bytes can be shared between recipients.
/// Cloning `bytes` only increments the `Arc` reference count.
#[derive(Clone, Debug)]
pub struct SharedPacketFrame {
    pub bytes: Arc<[u8]>,
}

impl SharedPacketFrame {
    /// The wire frame, including its length prefix and packet id.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Encodes one play packet once per protocol/compression variant, then shares
/// its immutable frame with each session via `Arc::clone`.
///
/// All recipients must use the same `VersionRouter` instance so identical
/// protocol numbers resolve to identical codec tables. A new packet requires
/// a new batch; the cache never outlives its borrowed packet.
pub struct BroadcastBatch<'a> {
    packet: &'a S2CPacket,
    frames: HashMap<BroadcastVariantKey, Arc<[u8]>>,
    router: Option<Arc<VersionRouter>>,
    stats: BroadcastBatchStats,
}

impl<'a> BroadcastBatch<'a> {
    pub fn new(packet: &'a S2CPacket) -> Self {
        Self {
            packet,
            frames: HashMap::new(),
            router: None,
            stats: BroadcastBatchStats::default(),
        }
    }

    /// Frame for a recipient in `Play`. A failed phase, router, or codec
    /// check returns `ProtoError` without recording a delivery or cache entry.
    pub fn frame_for(&mut self, session: &ServerSession) -> Result<SharedPacketFrame, ProtoError> {
        if !matches!(session.phase, SessionPhase::Play) {
            return Err(ProtoError::Malformed("broadcast outside the play phase"));
        }
        if let Some(router) = &self.router {
            if !Arc::ptr_eq(router, &session.config.router) {
                return Err(ProtoError::Malformed(
                    "broadcast recipients use different routers",
                ));
            }
        }
        let protocol_version = session.protocol.ok_or(ProtoError::Malformed(
            "broadcast without a negotiated version",
        ))?;
        if session.codec.is_none() {
            return Err(ProtoError::Malformed(
                "broadcast without a negotiated codec",
            ));
        }
        let key = BroadcastVariantKey {
            protocol_version,
            compression_threshold: session.config.compression_threshold.unwrap_or(-1),
        };
        let bytes = if let Some(cached) = self.frames.get(&key) {
            Arc::clone(cached)
        } else {
            let encoded = session.encode_s2c_checked(self.packet, PacketPhase::Play)?;
            let bytes: Arc<[u8]> = encoded.into();
            self.frames.insert(key, Arc::clone(&bytes));
            self.stats.unique_encodes += 1;
            bytes
        };
        if self.router.is_none() {
            self.router = Some(Arc::clone(&session.config.router));
        }
        self.stats.fanout_deliveries += 1;
        Ok(SharedPacketFrame { bytes })
    }

    pub fn stats(&self) -> BroadcastBatchStats {
        self.stats
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// A plain `{"text":"..."}` component in the anonymous-NBT wire shape the
/// disconnect packets carry — exported so embedding servers can build
/// `reject_login`/`disconnect` reasons without an NBT library.
pub fn text_component(text: &str) -> Vec<u8> {
    let text = truncate_utf8(text, 32_767);
    let mut v = Vec::with_capacity(text.len() + 16);
    v.push(10); // TAG_Compound
    v.push(8); // TAG_String
    v.extend_from_slice(&4u16.to_be_bytes());
    v.extend_from_slice(b"text");
    v.extend_from_slice(&(text.len() as u16).to_be_bytes());
    v.extend_from_slice(text.as_bytes());
    v.push(0); // TAG_End
    v
}

/// The `login_disconnect` frame for a failed negotiation — id `0x00` in both
/// target tables, emitted before any codec exists.
fn login_disconnect_frame(reason: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    write_frame(&mut out, ID_LOGIN_DISCONNECT, reason);
    out
}

/// The rejection reason carried by the unsupported-protocol
/// `login_disconnect`: names the requested number and every registered
/// version in `name (protocol N)` form (`1.21.1 (protocol 767)`, …).
fn unsupported_reason(requested: i32, supported: &[i32]) -> Vec<u8> {
    let mut message = format!("Unsupported protocol version {requested}; this server supports ");
    if supported.is_empty() {
        message.push_str("no registered protocol versions");
    } else {
        let names: Vec<String> = supported
            .iter()
            .map(|&v| ProtocolVersion::from_number(v).to_string())
            .collect();
        message.push_str(&names.join(", "));
    }
    text_component(&message)
}

/// Session-id entropy for the 26.x `login_success` trailer, drawn from the
/// randomized keys of `std`'s `RandomState` — keeps the crate dependency-
/// and clock-free while still producing an unpredictable id.
fn random_session_id() -> [u8; 16] {
    let a = RandomState::new().build_hasher().finish();
    let b = RandomState::new().build_hasher().finish();
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&a.to_be_bytes());
    id[8..].copy_from_slice(&b.to_be_bytes());
    id[6] = (id[6] & 0x0f) | 0x40; // RFC 4122 version 4
    id[8] = (id[8] & 0x3f) | 0x80; // RFC 4122 variant
    id
}

/// The JSON string-literal escapes the existing status reply applies.
fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Longest prefix of `s` within `max` bytes that ends on a char boundary.
fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(strict: bool) -> ServerSessionConfig {
        ServerSessionConfig {
            router: Arc::new(VersionRouter::with_v767_and_v777()),
            motd: "test".to_string(),
            max_players: 20,
            compression_threshold: None,
            strict_version_negotiation: strict,
        }
    }

    fn handshake_frame(protocol: i32, next_state: i32) -> Vec<u8> {
        let mut w = WireWriter::new();
        w.write_varint(protocol);
        w.write_string("localhost");
        w.write_u16(25565);
        w.write_varint(next_state);
        let mut out = Vec::new();
        write_frame(&mut out, 0x00, w.as_slice());
        out
    }

    #[test]
    fn text_component_is_wellformed_anonymous_nbt() {
        let c = text_component("hello");
        let mut r = WireReader::new(&c);
        let span = r.read_anonymous_nbt().unwrap();
        r.finish().unwrap();
        assert_eq!(span, &c[..]);
        // Compound holding one string named "text" plus the end tag.
        assert_eq!(c[0], 10);
        assert_eq!(&c[2..6], &[0, 4, b't', b'e']);
    }

    #[test]
    fn fresh_session_rejects_garbage_and_out_of_phase() {
        let mut s = ServerSession::new(cfg(true));
        assert!(matches!(
            s.on_frame(&[0xff, 0xff, 0xff]),
            Err(ProtoError::Malformed(_) | ProtoError::UnexpectedEof { .. })
        ));
        // A play-phase packet id is not a handshake.
        let mut w = Vec::new();
        write_frame(&mut w, 0x18, &[0; 8]);
        assert!(matches!(
            s.on_frame(&w),
            Err(ProtoError::UnknownPacketId { .. })
        ));
        assert!(s.accept_login([0; 16], "nobody", vec![]).is_err());
        assert!(s.send_play(&S2CPacket::KeepAlive { id: 1 }).is_err());
        assert!(s.finish_configuration().is_err());
        assert!(s.disconnect(b"x").is_err());
    }

    #[test]
    fn strict_status_ping_on_unknown_protocol_fails_closed() {
        let mut s = ServerSession::new(cfg(true));
        assert!(matches!(
            s.on_frame(&handshake_frame(754, 1)),
            Err(ProtoError::UnsupportedVersion { requested: 754, .. })
        ));
        assert!(s.is_closed());
        assert!(s.on_frame(&handshake_frame(767, 1)).is_err());
    }

    #[test]
    fn random_session_id_has_v4_shape_and_varies() {
        let a = random_session_id();
        let b = random_session_id();
        assert_ne!(a, b);
        assert_eq!(a[6] & 0xf0, 0x40);
        assert_eq!(a[8] & 0xc0, 0x80);
    }
}
