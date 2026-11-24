// SPDX-License-Identifier: LGPL-3.0-or-later
//! FRL2 friend-invite transport and a loopback adapter for the TCP game client.
//!
//! Every link opens by reading the relay's `RelayHello` nonce; afterwards the
//! host proves revokes with an HMAC bound to that nonce and guests join with a
//! `key_id`/`tag` pair — the invite code and the host's session key never
//! appear on the wire. See `megu_proto::frl_auth`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use megu_proto::frl_auth;
use megu_proto::frl_e2ee::{
    friend_host_accept, format_friend_id, random_32_bytes, FriendAllowlist,
    Frl3Cipher, Frl3FriendGuestHandshake, Frl3GuestToken, Frl3Identity, Frl3Message,
};
use megu_proto::friend_relay::{
    decode_frame, encode_frame, FriendPacket, InviteCode, MAX_FRL2_FRAME_BYTES,
    MAX_STREAM_PAYLOAD_BYTES,
};

const BRIDGE_POLL: Duration = Duration::from_millis(20);

/// Plaintext bytes per `FRL3` record on a friend stream: one record
/// (`u32be len | ciphertext | tag(16)`) must fit one `StreamData` payload.
const FRL3_CHUNK_BYTES: usize = MAX_STREAM_PAYLOAD_BYTES - 4 - 16;

struct FramedLink {
    stream: TcpStream,
    pending: Vec<u8>,
    /// Per-connection `RelayHello` nonce this link's `FRL2` proofs bind to.
    nonce: [u8; 16],
}

impl FramedLink {
    fn connect(relay_addr: &str, timeout: Duration) -> Result<Self, String> {
        let addresses: Vec<SocketAddr> = relay_addr
            .to_socket_addrs()
            .map_err(|e| format!("invalid relay address '{relay_addr}': {e}"))?
            .collect();
        if addresses.is_empty() {
            return Err(format!(
                "relay address '{relay_addr}' resolved to no addresses"
            ));
        }
        let mut last_error = None;
        for address in addresses {
            match TcpStream::connect_timeout(&address, timeout) {
                Ok(stream) => {
                    stream.set_nodelay(true).map_err(|e| e.to_string())?;
                    stream
                        .set_read_timeout(Some(timeout))
                        .map_err(|e| e.to_string())?;
                    stream
                        .set_write_timeout(Some(timeout))
                        .map_err(|e| e.to_string())?;
                    let mut link = Self {
                        stream,
                        pending: Vec::new(),
                        nonce: [0; 16],
                    };
                    // `RelayHello` is always the first frame the relay
                    // writes — read it before the caller builds any proofs.
                    match link.receive_required()? {
                        FriendPacket::RelayHello { nonce } => link.nonce = nonce,
                        packet => {
                            return Err(format!(
                                "expected relay hello, got {packet:?}"
                            ))
                        }
                    }
                    return Ok(link);
                }
                Err(e) => last_error = Some(e),
            }
        }
        Err(format!(
            "cannot connect to relay '{relay_addr}': {}",
            last_error.unwrap()
        ))
    }

    fn send(&mut self, packet: &FriendPacket) -> Result<(), String> {
        let frame = encode_frame(packet).map_err(|e| format!("invalid relay packet: {e}"))?;
        self.stream
            .write_all(&frame)
            .map_err(|e| format!("cannot send relay packet: {e}"))
    }

    // A timed-out read retains partial frames in `pending`, so the following
    // poll never loses the framing boundary on a slow or fragmented link.
    fn receive(&mut self, poll: bool) -> Result<Option<FriendPacket>, String> {
        loop {
            if self.pending.len() >= 4 {
                let len = u32::from_be_bytes(self.pending[..4].try_into().unwrap()) as usize;
                if !(5..=MAX_FRL2_FRAME_BYTES).contains(&len) {
                    return Err(format!("relay frame length {len} exceeds protocol bounds"));
                }
                if self.pending.len() >= len + 4 {
                    let frame: Vec<u8> = self.pending.drain(..len + 4).collect();
                    return decode_frame(&frame)
                        .map(Some)
                        .map_err(|e| format!("invalid relay frame: {e}"));
                }
            }
            let mut chunk = [0u8; 8192];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err("relay disconnected".to_string()),
                Ok(n) => self.pending.extend_from_slice(&chunk[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                    if poll {
                        return Ok(None);
                    }
                    return Err("timed out waiting for relay response".to_string());
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("cannot read relay packet: {e}")),
            }
        }
    }

    fn receive_required(&mut self) -> Result<FriendPacket, String> {
        self.receive(false)?
            .ok_or_else(|| "relay did not send a response".to_string())
    }

    fn set_read_timeout(&self, timeout: Duration) -> Result<(), String> {
        self.stream
            .set_read_timeout(Some(timeout))
            .map_err(|e| format!("cannot configure relay read timeout: {e}"))
    }
}

fn send_payload(link: &mut FramedLink, stream_id: u32, bytes: &[u8]) -> Result<(), String> {
    for chunk in bytes.chunks(MAX_STREAM_PAYLOAD_BYTES) {
        link.send(&FriendPacket::StreamData {
            stream_id,
            payload: chunk.to_vec(),
        })?;
    }
    Ok(())
}

/// The `FRL2` admission dance both guest shapes share: `GuestJoin` with
/// the nonce-bound `key_id`/`tag` proof (the invite code never travels),
/// returning the admitted `stream_id`.
fn guest_join(link: &mut FramedLink, code: &InviteCode, guest_name: &str) -> Result<u32, String> {
    let key_id = frl_auth::join_key_id(&code.as_bytes(), &link.nonce);
    let tag = frl_auth::join_tag(&code.as_bytes(), &key_id, guest_name);
    link.send(&FriendPacket::GuestJoin {
        key_id,
        guest_name: guest_name.to_string(),
        tag,
    })?;
    match link.receive_required()? {
        FriendPacket::GuestAccepted { stream_id } => Ok(stream_id),
        FriendPacket::GuestRejected { reason } => {
            Err(format!("friend invite rejected: {reason}"))
        }
        packet => Err(format!("unexpected relay join response: {packet:?}")),
    }
}

pub struct FriendGuestSession {
    link: FramedLink,
    stream_id: u32,
    closed: bool,
}

impl FriendGuestSession {
    pub fn connect(
        relay_addr: &str,
        code: &InviteCode,
        guest_name: &str,
        timeout: Duration,
    ) -> Result<Self, String> {
        let mut link = FramedLink::connect(relay_addr, timeout)?;
        let stream_id = guest_join(&mut link, code, guest_name)?;
        Ok(Self {
            link,
            stream_id,
            closed: false,
        })
    }

    pub fn stream_id(&self) -> u32 {
        self.stream_id
    }

    pub fn send_game_bytes(&mut self, bytes: &[u8]) -> Result<(), String> {
        if self.closed {
            return Err("friend stream is closed".to_string());
        }
        send_payload(&mut self.link, self.stream_id, bytes)
    }

    pub fn recv_game_bytes(&mut self) -> Result<Vec<u8>, String> {
        self.recv_game_bytes_inner(false)?
            .ok_or_else(|| "relay response missing".to_string())
    }

    fn recv_game_bytes_inner(&mut self, poll: bool) -> Result<Option<Vec<u8>>, String> {
        if self.closed {
            return Err("friend stream is closed".to_string());
        }
        loop {
            match self.link.receive(poll)? {
                Some(FriendPacket::StreamData { stream_id, payload })
                    if stream_id == self.stream_id =>
                {
                    return Ok(Some(payload))
                }
                Some(FriendPacket::StreamClose { stream_id, reason })
                    if stream_id == self.stream_id =>
                {
                    self.closed = true;
                    return Err(format!("friend stream closed: {reason}"));
                }
                Some(FriendPacket::HeartbeatAck { .. }) => continue,
                Some(packet) => {
                    self.closed = true;
                    return Err(format!(
                        "unexpected or foreign friend stream packet: {packet:?}"
                    ));
                }
                None => return Ok(None),
            }
        }
    }

    pub fn close(&mut self, reason: &str) -> Result<(), String> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.link.send(&FriendPacket::StreamClose {
            stream_id: self.stream_id,
            reason: reason.to_string(),
        })?;
        self.link
            .stream
            .shutdown(Shutdown::Both)
            .map_err(|e| e.to_string())
    }
}

/// The guest half of a `FRL3-FRIEND` session: same `FRL2` stream
/// admission as [`FriendGuestSession`], then the mutual static+ephemeral
/// Noise-IK handshake inside `StreamData` — every game byte afterwards is
/// a ChaCha20-Poly1305 record the relay can neither read nor forge.
struct FriendE2eeGuestSession {
    link: FramedLink,
    stream_id: u32,
    cipher: Frl3Cipher,
    closed: bool,
}

impl FriendE2eeGuestSession {
    fn connect(
        relay_addr: &str,
        code: &InviteCode,
        guest_name: &str,
        guest_identity: &Frl3Identity,
        expected_host_pk: &[u8; 32],
        token: Frl3GuestToken,
        timeout: Duration,
    ) -> Result<Self, String> {
        let mut link = FramedLink::connect(relay_addr, timeout)?;
        let stream_id = guest_join(&mut link, code, guest_name)?;
        // `FRL3-FRIEND` step 1: the guest's ephemeral + static keys and a
        // proof keyed by DH(s_g, s_h) — only reachable while holding the
        // pinned host public key's peer secret.
        let (handshake, hello) = Frl3FriendGuestHandshake::start(
            guest_identity,
            expected_host_pk,
            stream_id,
            guest_name,
            token,
        )
        .map_err(|e| format!("cannot start FRL3 friend handshake: {e}"))?;
        link.send(&FriendPacket::StreamData {
            stream_id,
            payload: Frl3Message::FriendHello(hello).encode(),
        })?;
        // Step 2: the host's `FriendAck` — `finish` verifies `host_confirm`
        // in constant time, proving the peer holds the host static secret.
        let cipher = loop {
            match link.receive_required()? {
                FriendPacket::StreamData {
                    stream_id: got,
                    payload,
                } if got == stream_id => match Frl3Message::decode(&payload) {
                    Ok(Frl3Message::FriendAck(ack)) => {
                        break handshake
                            .finish(&ack)
                            .map_err(|e| format!("FRL3 friend handshake failed: {e}"))?
                    }
                    _ => {
                        return Err(
                            "unexpected FRL3 handshake reply from host".to_string()
                        )
                    }
                },
                FriendPacket::StreamClose {
                    stream_id: got,
                    reason,
                } if got == stream_id => {
                    return Err(format!(
                        "friend stream closed during FRL3 handshake: {reason}"
                    ))
                }
                FriendPacket::HeartbeatAck { .. } => continue,
                packet => {
                    return Err(format!(
                        "unexpected packet during FRL3 handshake: {packet:?}"
                    ))
                }
            }
        };
        Ok(Self {
            link,
            stream_id,
            cipher,
            closed: false,
        })
    }

    fn sas_code(&self) -> u32 {
        self.cipher.sas_code()
    }

    fn send_game_bytes(&mut self, bytes: &[u8]) -> Result<(), String> {
        if self.closed {
            return Err("friend stream is closed".to_string());
        }
        for chunk in bytes.chunks(FRL3_CHUNK_BYTES) {
            let record = self
                .cipher
                .seal(chunk)
                .map_err(|e| format!("FRL3 record seal failed: {e}"))?;
            self.link.send(&FriendPacket::StreamData {
                stream_id: self.stream_id,
                payload: record,
            })?;
        }
        Ok(())
    }

    fn recv_game_bytes_inner(&mut self, poll: bool) -> Result<Option<Vec<u8>>, String> {
        if self.closed {
            return Err("friend stream is closed".to_string());
        }
        loop {
            match self.link.receive(poll)? {
                Some(FriendPacket::StreamData { stream_id, payload })
                    if stream_id == self.stream_id =>
                {
                    if Frl3Message::is_handshake(&payload) {
                        self.closed = true;
                        return Err(
                            "unexpected FRL3 handshake message on an open stream"
                                .to_string(),
                        );
                    }
                    return self
                        .cipher
                        .open(&payload)
                        .map(Some)
                        .map_err(|e| format!("FRL3 record rejected: {e}"));
                }
                Some(FriendPacket::StreamClose { stream_id, reason })
                    if stream_id == self.stream_id =>
                {
                    self.closed = true;
                    return Err(format!("friend stream closed: {reason}"));
                }
                Some(FriendPacket::HeartbeatAck { .. }) => continue,
                Some(packet) => {
                    self.closed = true;
                    return Err(format!(
                        "unexpected or foreign friend stream packet: {packet:?}"
                    ));
                }
                None => return Ok(None),
            }
        }
    }

    fn close(&mut self, reason: &str) -> Result<(), String> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.link.send(&FriendPacket::StreamClose {
            stream_id: self.stream_id,
            reason: reason.to_string(),
        })?;
        self.link
            .stream
            .shutdown(Shutdown::Both)
            .map_err(|e| e.to_string())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FriendHostEvent {
    GuestOpened { stream_id: u32, guest_name: String },
    GameData { stream_id: u32, payload: Vec<u8> },
    GuestClosed { stream_id: u32, reason: String },
}

/// `FRL3-FRIEND` state for an E2EE host session: the host's long-term
/// identity, the current allowlist snapshot, every established per-stream
/// cipher, and the streams still mid-handshake.
struct HostE2ee {
    /// Host's 256-bit static identity — proves `DH(s_h, ·)` to guests.
    identity: Frl3Identity,
    /// Trusted guest static keys; refreshed via `update_allowlist`.
    allowlist: FriendAllowlist,
    /// `stream_id` → established record-layer cipher.
    ciphers: BTreeMap<u32, Frl3Cipher>,
    /// `stream_id` → the guest's verified static key (for allowlist
    /// refresh kicks and `guest_friend_id`).
    guest_keys: BTreeMap<u32, [u8; 32]>,
    /// `stream_id` → `guest_name` for streams whose `StreamOpen` arrived
    /// but whose `FriendHello` has not yet completed.
    pending: BTreeMap<u32, String>,
}

impl HostE2ee {
    fn drop_stream(&mut self, stream_id: u32) {
        self.ciphers.remove(&stream_id);
        self.guest_keys.remove(&stream_id);
        self.pending.remove(&stream_id);
    }
}

pub struct FriendHostSession {
    link: FramedLink,
    invite_code: InviteCode,
    /// MAC key for `HostRevoke` proofs — sent once inside `HostRegister`.
    session_key: [u8; 16],
    streams: BTreeSet<u32>,
    revoked: bool,
    /// `Some` when registered via `register_e2ee_friends`.
    e2ee: Option<HostE2ee>,
}

impl fmt::Debug for FriendHostSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FriendHostSession")
            .field("invite_code", &self.invite_code)
            .field("session_key", &"[REDACTED]")
            .field("streams", &self.streams)
            .field("revoked", &self.revoked)
            .field("e2ee", &self.e2ee.is_some())
            .finish()
    }
}

impl FriendHostSession {
    /// Shared `HostRegister` dance for both host shapes.
    fn register_link(
        relay_addr: &str,
        protocol_version: i32,
        world_name: &str,
        max_guests: u8,
        session_key: [u8; 16],
        timeout: Duration,
    ) -> Result<(FramedLink, InviteCode), String> {
        let mut link = FramedLink::connect(relay_addr, timeout)?;
        link.send(&FriendPacket::HostRegister {
            protocol_version,
            world_name: world_name.to_string(),
            max_guests,
            session_key,
        })?;
        match link.receive_required()? {
            FriendPacket::HostRegistered { invite_code } => Ok((link, invite_code)),
            packet => Err(format!(
                "unexpected relay registration response: {packet:?}"
            )),
        }
    }

    pub fn register(
        relay_addr: &str,
        protocol_version: i32,
        world_name: &str,
        max_guests: u8,
        session_key: [u8; 16],
        timeout: Duration,
    ) -> Result<Self, String> {
        let (link, invite_code) = Self::register_link(
            relay_addr,
            protocol_version,
            world_name,
            max_guests,
            session_key,
            timeout,
        )?;
        Ok(Self {
            link,
            invite_code,
            session_key,
            streams: BTreeSet::new(),
            revoked: false,
            e2ee: None,
        })
    }

    /// Register like [`register`](Self::register) but with the `FRL3-FRIEND`
    /// zero-trust layer: every accepted guest stream must complete the
    /// mutual static+ephemeral X25519 handshake and prove its static key
    /// is on `allowlist` before a single game byte flows. Afterwards
    /// `send_game_bytes`/`GameData` move AEAD-sealed records — the relay
    /// only ever forwards ciphertext. The `HostRevoke` session key is a
    /// fresh CSPRNG draw, never caller-chosen.
    pub fn register_e2ee_friends(
        relay_addr: &str,
        protocol_version: i32,
        world_name: &str,
        max_guests: u8,
        host_identity: Frl3Identity,
        allowlist: FriendAllowlist,
        timeout: Duration,
    ) -> Result<Self, String> {
        let mut session_key = [0u8; 16];
        session_key.copy_from_slice(&random_32_bytes()[..16]);
        let (link, invite_code) = Self::register_link(
            relay_addr,
            protocol_version,
            world_name,
            max_guests,
            session_key,
            timeout,
        )?;
        Ok(Self {
            link,
            invite_code,
            session_key,
            streams: BTreeSet::new(),
            revoked: false,
            e2ee: Some(HostE2ee {
                identity: host_identity,
                allowlist,
                ciphers: BTreeMap::new(),
                guest_keys: BTreeMap::new(),
                pending: BTreeMap::new(),
            }),
        })
    }

    pub fn invite_code(&self) -> InviteCode {
        self.invite_code
    }

    /// Refresh the `FRL3-FRIEND` allowlist mid-session: friends removed
    /// since the last snapshot are instantly disconnected (`StreamClose`),
    /// and future handshakes are gated on the new set. Returns the number
    /// of established guests kicked by the refresh.
    pub fn update_allowlist(&mut self, allowlist: FriendAllowlist) -> Result<usize, String> {
        if self.e2ee.is_none() {
            return Err("update_allowlist requires a FRL3 friend host session".to_string());
        }
        let kicked: Vec<u32> = {
            let e2ee = self.e2ee.as_mut().unwrap();
            e2ee.allowlist = allowlist;
            e2ee
                .guest_keys
                .iter()
                .filter(|(_, pk)| !e2ee.allowlist.contains(pk))
                .map(|(&stream_id, _)| stream_id)
                .collect()
        };
        let count = kicked.len();
        for stream_id in kicked {
            self.terminate_stream(stream_id, "removed from the friends allowlist");
        }
        Ok(count)
    }

    /// The verified `megu-id:` of an established `FRL3-FRIEND` guest —
    /// the identity the handshake actually proved, never the (spoofable)
    /// `guest_name`. `None` for legacy sessions or unfinished handshakes.
    pub fn guest_friend_id(&self, stream_id: u32) -> Option<String> {
        self.e2ee
            .as_ref()
            .and_then(|e| e.guest_keys.get(&stream_id))
            .map(format_friend_id)
    }

    /// Close `stream_id` with `reason` and drop all per-stream state —
    /// shared by E2EE rejections, allowlist kicks and `close_stream`.
    fn terminate_stream(&mut self, stream_id: u32, reason: &str) {
        self.streams.remove(&stream_id);
        if let Some(e2ee) = &mut self.e2ee {
            e2ee.drop_stream(stream_id);
        }
        let _ = self.link.send(&FriendPacket::StreamClose {
            stream_id,
            reason: reason.to_string(),
        });
    }

    /// Handle one `StreamData` payload on an E2EE host session: either the
    /// guest's `FriendHello` (→ `friend_host_accept`, `FriendAck` reply,
    /// `GuestOpened` on success — any rejection is fail-closed and the
    /// stream is terminated) or an AEAD record (→ `GameData` plaintext).
    fn recv_e2ee_stream_data(
        &mut self,
        stream_id: u32,
        payload: Vec<u8>,
    ) -> Result<FriendHostEvent, String> {
        if Frl3Message::is_handshake(&payload) {
            let Some(guest_name) = self
                .e2ee
                .as_ref()
                .and_then(|e| e.pending.get(&stream_id).cloned())
            else {
                self.terminate_stream(
                    stream_id,
                    "FRL3 handshake message on an established stream",
                );
                return Ok(FriendHostEvent::GuestClosed {
                    stream_id,
                    reason: "FRL3 handshake replay rejected".to_string(),
                });
            };
            let hello = match Frl3Message::decode(&payload) {
                Ok(Frl3Message::FriendHello(hello)) => hello,
                _ => {
                    self.terminate_stream(stream_id, "malformed FRL3 friend hello");
                    return Ok(FriendHostEvent::GuestClosed {
                        stream_id,
                        reason: "malformed FRL3 friend hello".to_string(),
                    });
                }
            };
            let e2ee = self.e2ee.as_ref().unwrap();
            match friend_host_accept(
                &e2ee.identity,
                &e2ee.allowlist,
                stream_id,
                &guest_name,
                &hello,
            ) {
                Ok((cipher, ack)) => {
                    self.link.send(&FriendPacket::StreamData {
                        stream_id,
                        payload: Frl3Message::FriendAck(ack).encode(),
                    })?;
                    let e2ee = self.e2ee.as_mut().unwrap();
                    e2ee.pending.remove(&stream_id);
                    e2ee.guest_keys.insert(stream_id, hello.guest_static_pk);
                    e2ee.ciphers.insert(stream_id, cipher);
                    Ok(FriendHostEvent::GuestOpened {
                        stream_id,
                        guest_name,
                    })
                }
                Err(e) => {
                    let reason = format!("FRL3 friend handshake rejected: {e}");
                    self.terminate_stream(stream_id, &reason);
                    Ok(FriendHostEvent::GuestClosed { stream_id, reason })
                }
            }
        } else {
            let e2ee = self.e2ee.as_mut().unwrap();
            if e2ee.pending.contains_key(&stream_id) {
                let reason = "game data before the FRL3 handshake completed".to_string();
                self.terminate_stream(stream_id, &reason);
                return Ok(FriendHostEvent::GuestClosed { stream_id, reason });
            }
            match e2ee.ciphers.get_mut(&stream_id) {
                Some(cipher) => match cipher.open(&payload) {
                    Ok(plaintext) => Ok(FriendHostEvent::GameData {
                        stream_id,
                        payload: plaintext,
                    }),
                    Err(e) => {
                        let reason = format!("FRL3 record rejected: {e}");
                        self.terminate_stream(stream_id, &reason);
                        Ok(FriendHostEvent::GuestClosed { stream_id, reason })
                    }
                },
                None => {
                    let reason = "FRL3 record for an unestablished stream".to_string();
                    self.terminate_stream(stream_id, &reason);
                    Ok(FriendHostEvent::GuestClosed { stream_id, reason })
                }
            }
        }
    }

    pub fn recv_event(&mut self) -> Result<FriendHostEvent, String> {
        if self.revoked {
            return Err("friend invite has been revoked".to_string());
        }
        loop {
            match self.link.receive_required()? {
                FriendPacket::StreamOpen {
                    stream_id,
                    guest_name,
                } => {
                    if !self.streams.insert(stream_id) {
                        return Err(format!("duplicate friend stream id {stream_id}"));
                    }
                    if let Some(e2ee) = &mut self.e2ee {
                        // `GuestOpened` is only emitted once the FRL3
                        // handshake proves the guest's identity — an
                        // unauthenticated stream never "opens".
                        e2ee.pending.insert(stream_id, guest_name);
                        continue;
                    }
                    return Ok(FriendHostEvent::GuestOpened {
                        stream_id,
                        guest_name,
                    });
                }
                FriendPacket::StreamData { stream_id, payload } => {
                    if !self.streams.contains(&stream_id) {
                        return Err(format!("unknown friend stream id {stream_id}"));
                    }
                    if self.e2ee.is_some() {
                        return self.recv_e2ee_stream_data(stream_id, payload);
                    }
                    return Ok(FriendHostEvent::GameData { stream_id, payload });
                }
                FriendPacket::StreamClose { stream_id, reason } => {
                    if !self.streams.remove(&stream_id) {
                        return Err(format!("unknown friend stream id {stream_id}"));
                    }
                    if let Some(e2ee) = &mut self.e2ee {
                        e2ee.drop_stream(stream_id);
                    }
                    return Ok(FriendHostEvent::GuestClosed { stream_id, reason });
                }
                FriendPacket::HeartbeatAck { .. } => continue,
                packet => return Err(format!("unexpected host relay packet: {packet:?}")),
            }
        }
    }

    pub fn send_game_bytes(&mut self, stream_id: u32, bytes: &[u8]) -> Result<(), String> {
        if self.revoked || !self.streams.contains(&stream_id) {
            return Err(format!("friend stream {stream_id} is not open"));
        }
        if let Some(e2ee) = &mut self.e2ee {
            let Some(cipher) = e2ee.ciphers.get_mut(&stream_id) else {
                return Err(format!(
                    "friend stream {stream_id} has no established FRL3 session"
                ));
            };
            for chunk in bytes.chunks(FRL3_CHUNK_BYTES) {
                let record = cipher
                    .seal(chunk)
                    .map_err(|e| format!("FRL3 record seal failed: {e}"))?;
                self.link.send(&FriendPacket::StreamData {
                    stream_id,
                    payload: record,
                })?;
            }
            return Ok(());
        }
        send_payload(&mut self.link, stream_id, bytes)
    }

    pub fn close_stream(&mut self, stream_id: u32, reason: &str) -> Result<(), String> {
        if !self.streams.remove(&stream_id) {
            return Err(format!("friend stream {stream_id} is not open"));
        }
        if let Some(e2ee) = &mut self.e2ee {
            e2ee.drop_stream(stream_id);
        }
        self.link.send(&FriendPacket::StreamClose {
            stream_id,
            reason: reason.to_string(),
        })
    }

    pub fn revoke(&mut self) -> Result<(), String> {
        if self.revoked {
            return Ok(());
        }
        let mac = frl_auth::revoke_mac(
            &self.session_key,
            &self.invite_code.as_bytes(),
            &self.link.nonce,
        );
        self.link.send(&FriendPacket::HostRevoke {
            invite_code: self.invite_code,
            mac,
        })?;
        loop {
            match self.link.receive_required()? {
                FriendPacket::HostRevoked { ok: true } => {
                    self.revoked = true;
                    self.streams.clear();
                    if let Some(e2ee) = &mut self.e2ee {
                        e2ee.ciphers.clear();
                        e2ee.guest_keys.clear();
                        e2ee.pending.clear();
                    }
                    return Ok(());
                }
                FriendPacket::HostRevoked { ok: false } => {
                    return Err("relay refused to revoke friend invite".to_string());
                }
                FriendPacket::StreamClose { stream_id, .. } => {
                    self.streams.remove(&stream_id);
                    if let Some(e2ee) = &mut self.e2ee {
                        e2ee.drop_stream(stream_id);
                    }
                }
                FriendPacket::StreamOpen { stream_id, .. } => {
                    self.streams.insert(stream_id);
                }
                FriendPacket::StreamData { .. } | FriendPacket::HeartbeatAck { .. } => {}
                packet => return Err(format!("unexpected relay revoke response: {packet:?}")),
            }
        }
    }
}

/// The relay-side half a [`FriendGuestBridge`] worker pumps: plaintext in
/// and out regardless of whether the link is legacy `FRL2` or an
/// `FRL3-FRIEND` E2EE tunnel.
trait BridgePipe: Send {
    fn send_game_bytes(&mut self, bytes: &[u8]) -> Result<(), String>;
    fn poll_game_bytes(&mut self) -> Result<Option<Vec<u8>>, String>;
    fn set_poll_timeout(&mut self, timeout: Duration) -> Result<(), String>;
    fn close(&mut self, reason: &str);
}

impl BridgePipe for FriendGuestSession {
    fn send_game_bytes(&mut self, bytes: &[u8]) -> Result<(), String> {
        FriendGuestSession::send_game_bytes(self, bytes)
    }
    fn poll_game_bytes(&mut self) -> Result<Option<Vec<u8>>, String> {
        self.recv_game_bytes_inner(true)
    }
    fn set_poll_timeout(&mut self, timeout: Duration) -> Result<(), String> {
        self.link.set_read_timeout(timeout)
    }
    fn close(&mut self, reason: &str) {
        let _ = FriendGuestSession::close(self, reason);
    }
}

impl BridgePipe for FriendE2eeGuestSession {
    fn send_game_bytes(&mut self, bytes: &[u8]) -> Result<(), String> {
        FriendE2eeGuestSession::send_game_bytes(self, bytes)
    }
    fn poll_game_bytes(&mut self) -> Result<Option<Vec<u8>>, String> {
        self.recv_game_bytes_inner(true)
    }
    fn set_poll_timeout(&mut self, timeout: Duration) -> Result<(), String> {
        self.link.set_read_timeout(timeout)
    }
    fn close(&mut self, reason: &str) {
        let _ = FriendE2eeGuestSession::close(self, reason);
    }
}

/// One-session local TCP endpoint usable by the existing Minecraft client.
/// The relay is joined before the endpoint is returned, so rejection fails
/// before the game's normal handshake begins.
pub struct FriendGuestBridge {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<(), String>>>,
    /// The `FRL3-FRIEND` transcript SAS for pairing verification — `Some`
    /// only for bridges opened via `start_e2ee_friend*`.
    sas_code: Option<u32>,
}

impl FriendGuestBridge {
    /// The shared loopback-worker setup for both guest shapes: bind an
    /// ephemeral loopback listener, then shuffle bytes between the first
    /// accepted local connection and the pipe until either side closes.
    fn spawn<P: BridgePipe + 'static>(
        mut guest: P,
        timeout: Duration,
        sas_code: Option<u32>,
    ) -> Result<Self, String> {
        guest.set_poll_timeout(BRIDGE_POLL)?;
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|e| format!("cannot bind friend loopback: {e}"))?;
        let address = listener.local_addr().map_err(|e| e.to_string())?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || -> Result<(), String> {
            let mut local = loop {
                if worker_stop.load(Ordering::Relaxed) {
                    guest.close("client stopped");
                    return Ok(());
                }
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => thread::sleep(BRIDGE_POLL),
                    Err(e) => return Err(format!("friend loopback accept failed: {e}")),
                }
            };
            local
                .set_read_timeout(Some(BRIDGE_POLL))
                .map_err(|e| e.to_string())?;
            local
                .set_write_timeout(Some(timeout))
                .map_err(|e| e.to_string())?;
            let _ = local.set_nodelay(true);
            let mut buffer = [0u8; 8192];
            let result = loop {
                if worker_stop.load(Ordering::Relaxed) {
                    break Ok(());
                }
                match local.read(&mut buffer) {
                    Ok(0) => break Ok(()),
                    Ok(n) => {
                        if let Err(err) = guest.send_game_bytes(&buffer[..n]) {
                            break Err(err);
                        }
                    }
                    Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {}
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(e) => break Err(format!("friend loopback read failed: {e}")),
                }
                match guest.poll_game_bytes() {
                    Ok(Some(bytes)) => {
                        if let Err(e) = local.write_all(&bytes) {
                            break Err(format!("friend loopback write failed: {e}"));
                        }
                    }
                    Ok(None) => {}
                    Err(e) => break Err(e),
                }
            };
            guest.close("client disconnected");
            let _ = local.shutdown(Shutdown::Both);
            result
        });
        Ok(Self {
            address,
            stop,
            worker: Some(worker),
            sas_code,
        })
    }

    pub fn start(
        relay_addr: &str,
        code: &InviteCode,
        guest_name: &str,
        timeout: Duration,
    ) -> Result<Self, String> {
        let guest = FriendGuestSession::connect(relay_addr, code, guest_name, timeout)?;
        Self::spawn(guest, timeout, None)
    }

    /// Open a `FRL3-FRIEND` bridge: the mutual static-identity handshake
    /// proves this client's [`Frl3Identity`] to the host and pins the
    /// host's `expected_host_pk` (its `megu-id:`) so a MITM — including
    /// the relay itself — cannot impersonate either side. All game bytes
    /// then travel as AEAD records inside `StreamData`, bridged
    /// transparently to loopback TCP for the Minecraft client.
    pub fn start_e2ee_friend(
        relay_addr: &str,
        code: &InviteCode,
        guest_name: &str,
        guest_identity: &Frl3Identity,
        expected_host_pk: &[u8; 32],
        timeout: Duration,
    ) -> Result<Self, String> {
        Self::start_e2ee_friend_with_token(
            relay_addr,
            code,
            guest_name,
            guest_identity,
            expected_host_pk,
            Frl3GuestToken::NONE,
            timeout,
        )
    }

    /// [`start_e2ee_friend`](Self::start_e2ee_friend) additionally
    /// presenting a 128-bit one-off [`Frl3GuestToken`] for hosts whose
    /// allowlist entry requires one.
    pub fn start_e2ee_friend_with_token(
        relay_addr: &str,
        code: &InviteCode,
        guest_name: &str,
        guest_identity: &Frl3Identity,
        expected_host_pk: &[u8; 32],
        token: Frl3GuestToken,
        timeout: Duration,
    ) -> Result<Self, String> {
        let guest = FriendE2eeGuestSession::connect(
            relay_addr,
            code,
            guest_name,
            guest_identity,
            expected_host_pk,
            token,
            timeout,
        )?;
        let sas_code = guest.sas_code();
        Self::spawn(guest, timeout, Some(sas_code))
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    /// The six-digit `FRL3-FRIEND` transcript fingerprint — compare with
    /// the host's when pairing a new friend. `None` for legacy `FRL2`
    /// bridges.
    pub fn sas_code(&self) -> Option<u32> {
        self.sas_code
    }
}

impl Drop for FriendGuestBridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    use megu_proto::friend_relay::TcpFriendRelay;

    const TIMEOUT: Duration = Duration::from_secs(10);

    /// A real `TcpFriendRelay` driven on a background thread — the same
    /// loopback fixture the `friend_and_sync` integration tests use.
    struct TestRelay {
        addr: String,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl TestRelay {
        fn start() -> Self {
            let mut relay = TcpFriendRelay::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = relay.local_addr().unwrap().to_string();
            let stop = Arc::new(AtomicBool::new(false));
            let running = Arc::clone(&stop);
            let thread = thread::spawn(move || {
                while !running.load(Ordering::Relaxed) {
                    let _ = relay.poll_once(Instant::now());
                    thread::sleep(Duration::from_millis(1));
                }
            });
            Self {
                addr,
                stop,
                thread: Some(thread),
            }
        }
    }

    impl Drop for TestRelay {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn e2ee_host(relay: &TestRelay, guests: &[&Frl3Identity]) -> (FriendHostSession, Frl3Identity) {
        let host_id = Frl3Identity::generate();
        let mut allowlist = FriendAllowlist::new();
        for guest in guests {
            allowlist.add(guest.public_key());
        }
        let host = FriendHostSession::register_e2ee_friends(
            &relay.addr,
            767,
            "E2EE World",
            4,
            host_id.clone(),
            allowlist,
            TIMEOUT,
        )
        .unwrap();
        (host, host_id)
    }

    #[test]
    fn e2ee_friend_bridge_exchanges_cleartext_over_encrypted_tunnel() {
        let relay = TestRelay::start();
        let guest_id = Frl3Identity::generate();
        let (mut host, host_id) = e2ee_host(&relay, &[&guest_id]);
        let host_pk = host_id.public_key();
        let code = host.invite_code();
        let addr = relay.addr.clone();

        // The guest bridge completes the mutual handshake while the host
        // answers it inside recv_event.
        let join = thread::spawn(move || {
            FriendGuestBridge::start_e2ee_friend(&addr, &code, "Alice", &guest_id, &host_pk, TIMEOUT)
        });
        let stream_id = match host.recv_event().unwrap() {
            FriendHostEvent::GuestOpened {
                stream_id,
                guest_name,
            } => {
                assert_eq!(guest_name, "Alice");
                stream_id
            }
            event => panic!("expected GuestOpened: {event:?}"),
        };
        let bridge = join.join().unwrap().unwrap();
        // The verified friend_id is the guest's real identity, and both
        // sides agree on the same transcript SAS for pairing checks.
        assert!(bridge.sas_code().is_some());

        // Guest → host: plaintext in at the loopback end, plaintext out
        // of the host's GameData — encrypted records in between.
        let mut local = TcpStream::connect(bridge.local_addr()).unwrap();
        local.set_read_timeout(Some(TIMEOUT)).unwrap();
        local.write_all(b"hello over e2ee").unwrap();
        match host.recv_event().unwrap() {
            FriendHostEvent::GameData { stream_id: s, payload } => {
                assert_eq!(s, stream_id);
                assert_eq!(payload, b"hello over e2ee");
            }
            event => panic!("expected GameData: {event:?}"),
        }

        // Host → guest: same opacity in the other direction.
        host.send_game_bytes(stream_id, b"reply over e2ee").unwrap();
        let mut buf = [0u8; 15];
        local.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"reply over e2ee");

        drop(local);
        drop(bridge);
        host.revoke().unwrap();
    }

    #[test]
    fn e2ee_unlisted_friend_key_is_rejected_fail_closed() {
        let relay = TestRelay::start();
        let listed = Frl3Identity::generate();
        let stranger = Frl3Identity::generate();
        let (mut host, host_id) = e2ee_host(&relay, &[&listed]);
        let host_pk = host_id.public_key();
        let code = host.invite_code();
        let addr = relay.addr.clone();

        // A guest holding a valid 256-bit identity that is simply not on
        // the allowlist: the host must reject the handshake and never
        // emit GuestOpened.
        let join = thread::spawn(move || {
            FriendGuestBridge::start_e2ee_friend(&addr, &code, "Mallory", &stranger, &host_pk, TIMEOUT)
        });
        match host.recv_event().unwrap() {
            FriendHostEvent::GuestClosed { reason, .. } => {
                assert!(
                    reason.contains("FRL3") || reason.contains("allowlist"),
                    "unexpected rejection reason: {reason}"
                );
            }
            event => panic!("expected GuestClosed for unlisted guest: {event:?}"),
        }
        assert!(join.join().unwrap().is_err());
        host.revoke().unwrap();
    }

    #[test]
    fn e2ee_allowlist_refresh_disconnects_and_gates_rejoin() {
        let relay = TestRelay::start();
        let guest_id = Frl3Identity::generate();
        let (mut host, host_id) = e2ee_host(&relay, &[&guest_id]);
        let host_pk = host_id.public_key();
        let code = host.invite_code();

        // Join once successfully.
        let addr = relay.addr.clone();
        let join_guest = guest_id.clone();
        let join = thread::spawn(move || {
            FriendGuestBridge::start_e2ee_friend(&addr, &code, "Alice", &join_guest, &host_pk, TIMEOUT)
        });
        let stream_id = match host.recv_event().unwrap() {
            FriendHostEvent::GuestOpened { stream_id, .. } => stream_id,
            event => panic!("expected GuestOpened: {event:?}"),
        };
        assert_eq!(
            host.guest_friend_id(stream_id).as_deref(),
            Some(guest_id.friend_id().as_str())
        );
        let bridge = join.join().unwrap().unwrap();
        let mut local = TcpStream::connect(bridge.local_addr()).unwrap();
        local.set_read_timeout(Some(TIMEOUT)).unwrap();

        // Removing the friend from the allowlist kicks the live session
        // instantly — the loopback end sees the tunnel collapse.
        let kicked = host.update_allowlist(FriendAllowlist::new()).unwrap();
        assert_eq!(kicked, 1);
        assert_eq!(host.guest_friend_id(stream_id), None);
        let mut byte = [0u8; 1];
        assert_eq!(local.read(&mut byte).unwrap(), 0, "kicked guest must see EOF");

        // And the same identity can no longer rejoin.
        let addr = relay.addr.clone();
        let join_guest = guest_id.clone();
        let join = thread::spawn(move || {
            FriendGuestBridge::start_e2ee_friend(&addr, &code, "Alice", &join_guest, &host_pk, TIMEOUT)
        });
        match host.recv_event().unwrap() {
            FriendHostEvent::GuestClosed { .. } => {}
            event => panic!("expected GuestClosed for revoked friend: {event:?}"),
        }
        assert!(join.join().unwrap().is_err());

        // Re-adding the friend restores admission.
        let mut restored = FriendAllowlist::new();
        restored.add(guest_id.public_key());
        assert_eq!(host.update_allowlist(restored).unwrap(), 0);
        let addr = relay.addr.clone();
        let join = thread::spawn(move || {
            FriendGuestBridge::start_e2ee_friend(&addr, &code, "Alice", &guest_id, &host_pk, TIMEOUT)
        });
        match host.recv_event().unwrap() {
            FriendHostEvent::GuestOpened { guest_name, .. } => {
                assert_eq!(guest_name, "Alice")
            }
            event => panic!("expected GuestOpened after re-add: {event:?}"),
        }
        let _bridge = join.join().unwrap().unwrap();
        host.revoke().unwrap();
    }

    #[test]
    fn update_allowlist_requires_e2ee_session() {
        let relay = TestRelay::start();
        let mut host =
            FriendHostSession::register(&relay.addr, 767, "Legacy", 2, [57; 16], TIMEOUT).unwrap();
        assert!(host.update_allowlist(FriendAllowlist::new()).is_err());
        host.revoke().unwrap();
    }
}
