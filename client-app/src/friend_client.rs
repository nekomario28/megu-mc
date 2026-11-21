// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! FRL1 friend-invite transport and a loopback adapter for the TCP game client.

use std::collections::BTreeSet;
use std::fmt;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use mc_spine_proto::friend_relay::{
    decode_frame, encode_frame, FriendPacket, InviteCode, MAX_FRL1_FRAME_BYTES,
    MAX_STREAM_PAYLOAD_BYTES,
};

const BRIDGE_POLL: Duration = Duration::from_millis(20);

struct FramedLink {
    stream: TcpStream,
    pending: Vec<u8>,
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
                    return Ok(Self {
                        stream,
                        pending: Vec::new(),
                    });
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
                if !(5..=MAX_FRL1_FRAME_BYTES).contains(&len) {
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
        link.send(&FriendPacket::GuestJoin {
            invite_code: *code,
            guest_name: guest_name.to_string(),
        })?;
        match link.receive_required()? {
            FriendPacket::GuestAccepted { stream_id } => Ok(Self {
                link,
                stream_id,
                closed: false,
            }),
            FriendPacket::GuestRejected { reason } => {
                Err(format!("friend invite rejected: {reason}"))
            }
            packet => Err(format!("unexpected relay join response: {packet:?}")),
        }
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

#[derive(Debug, PartialEq, Eq)]
pub enum FriendHostEvent {
    GuestOpened { stream_id: u32, guest_name: String },
    GameData { stream_id: u32, payload: Vec<u8> },
    GuestClosed { stream_id: u32, reason: String },
}

pub struct FriendHostSession {
    link: FramedLink,
    invite_code: InviteCode,
    host_secret: [u8; 16],
    streams: BTreeSet<u32>,
    revoked: bool,
}

impl fmt::Debug for FriendHostSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FriendHostSession")
            .field("invite_code", &self.invite_code)
            .field("host_secret", &"[REDACTED]")
            .field("streams", &self.streams)
            .field("revoked", &self.revoked)
            .finish()
    }
}

impl FriendHostSession {
    pub fn register(
        relay_addr: &str,
        protocol_version: i32,
        world_name: &str,
        max_guests: u8,
        host_secret: [u8; 16],
        timeout: Duration,
    ) -> Result<Self, String> {
        let mut link = FramedLink::connect(relay_addr, timeout)?;
        link.send(&FriendPacket::HostRegister {
            protocol_version,
            world_name: world_name.to_string(),
            max_guests,
            host_secret,
        })?;
        match link.receive_required()? {
            FriendPacket::HostRegistered { invite_code } => Ok(Self {
                link,
                invite_code,
                host_secret,
                streams: BTreeSet::new(),
                revoked: false,
            }),
            packet => Err(format!(
                "unexpected relay registration response: {packet:?}"
            )),
        }
    }

    pub fn invite_code(&self) -> InviteCode {
        self.invite_code
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
                    return Ok(FriendHostEvent::GuestOpened {
                        stream_id,
                        guest_name,
                    });
                }
                FriendPacket::StreamData { stream_id, payload } => {
                    if !self.streams.contains(&stream_id) {
                        return Err(format!("unknown friend stream id {stream_id}"));
                    }
                    return Ok(FriendHostEvent::GameData { stream_id, payload });
                }
                FriendPacket::StreamClose { stream_id, reason } => {
                    if !self.streams.remove(&stream_id) {
                        return Err(format!("unknown friend stream id {stream_id}"));
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
        send_payload(&mut self.link, stream_id, bytes)
    }

    pub fn close_stream(&mut self, stream_id: u32, reason: &str) -> Result<(), String> {
        if !self.streams.remove(&stream_id) {
            return Err(format!("friend stream {stream_id} is not open"));
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
        self.link.send(&FriendPacket::HostRevoke {
            invite_code: self.invite_code,
            host_secret: self.host_secret,
        })?;
        loop {
            match self.link.receive_required()? {
                FriendPacket::HostRevoked { ok: true } => {
                    self.revoked = true;
                    self.streams.clear();
                    return Ok(());
                }
                FriendPacket::HostRevoked { ok: false } => {
                    return Err("relay refused to revoke friend invite".to_string());
                }
                FriendPacket::StreamClose { stream_id, .. } => {
                    self.streams.remove(&stream_id);
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

/// One-session local TCP endpoint usable by the existing Minecraft client.
/// The relay is joined before the endpoint is returned, so rejection fails
/// before the game's normal handshake begins.
pub struct FriendGuestBridge {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<(), String>>>,
}

impl FriendGuestBridge {
    pub fn start(
        relay_addr: &str,
        code: &InviteCode,
        guest_name: &str,
        timeout: Duration,
    ) -> Result<Self, String> {
        let mut guest = FriendGuestSession::connect(relay_addr, code, guest_name, timeout)?;
        guest.link.set_read_timeout(BRIDGE_POLL)?;
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|e| format!("cannot bind friend loopback: {e}"))?;
        let address = listener.local_addr().map_err(|e| e.to_string())?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || -> Result<(), String> {
            let mut local = loop {
                if worker_stop.load(Ordering::Relaxed) {
                    let _ = guest.close("client stopped");
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
                match guest.recv_game_bytes_inner(true) {
                    Ok(Some(bytes)) => {
                        if let Err(e) = local.write_all(&bytes) {
                            break Err(format!("friend loopback write failed: {e}"));
                        }
                    }
                    Ok(None) => {}
                    Err(e) => break Err(e),
                }
            };
            let _ = guest.close("client disconnected");
            let _ = local.shutdown(Shutdown::Both);
            result
        });
        Ok(Self {
            address,
            stop,
            worker: Some(worker),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.address
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
