// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Asynchronous live Protocol 767/777 session and renderer-facing world state.
//! Socket operations happen on a worker thread; `poll` never performs I/O.

use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::time::{Duration, Instant};

use mc_spine_client::protocol::{
    offline_uuid, ClientCustomChannelEvent, ClientPlayEvent, DecodedMagicCastState,
    DecodedMagicSync, DecodedMotionTrigger, DimensionMeta, PlayEvent, ProtocolClient, TeleportSync,
};
use mc_spine_client::render::ClientVfxHost;
use mc_spine_client::render_graph::GpuPipelineKind;
use mc_spine_client::world::{BlockTable, WorldState};
use mc_spine_ui::magic_hud::{CastBarState, CooldownSlotState, MagicHudState, StatusBadgeState};
use mc_spine_ui::state::HudState;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionStatus {
    Connecting,
    Playing,
    Disconnected(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlayerPose {
    /// Feet position in Minecraft world coordinates.
    pub feet: [f64; 3],
    pub yaw: f32,
    pub pitch: f32,
}

impl PlayerPose {
    pub fn from_teleport(teleport: &TeleportSync, current: Option<Self>) -> Self {
        let previous = current.unwrap_or(Self {
            feet: [0.0; 3],
            yaw: 0.0,
            pitch: 0.0,
        });
        let mut feet = [teleport.x, teleport.y, teleport.z];
        for (index, component) in feet.iter_mut().enumerate() {
            if teleport.flags & (1 << index) != 0 {
                *component += previous.feet[index];
            }
        }
        Self {
            feet,
            yaw: teleport.yaw
                + if teleport.flags & 8 != 0 {
                    previous.yaw
                } else {
                    0.0
                },
            pitch: teleport.pitch
                + if teleport.flags & 16 != 0 {
                    previous.pitch
                } else {
                    0.0
                },
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PlayerMovement {
    pub pose: PlayerPose,
    pub strafe: f32,
    pub forward: f32,
    pub jump: bool,
}

enum WorkerMessage {
    Connected,
    Play(ClientPlayEvent),
    Closed(String),
}

enum WorkerCommand {
    Movement(PlayerMovement),
}

/// Owns UI-facing world data on the event-loop thread. The worker owns its socket.
pub struct LiveSession {
    incoming: Receiver<WorkerMessage>,
    outgoing: SyncSender<WorkerCommand>,
    pub status: SessionStatus,
    pub world: WorldState,
    pub hud: HudState,
    pub magic: MagicHudState,
    pub vfx: ClientVfxHost,
    pub motion: Option<DecodedMotionTrigger>,
    pub pose: Option<PlayerPose>,
    pub camera_sync: Option<PlayerPose>,
    pub terrain_dirty: bool,
    pub vfx_admitted: u64,
    pub vfx_rejected: u64,
    pub animation_packets: u64,
    pub animation_rejected: u64,
    pub motion_packets: u64,
    pub chat_message: Option<String>,
    last_vfx_tick: Instant,
    last_animation_tick: Instant,
}

impl LiveSession {
    pub fn connect(host: String, port: u16, username: String, version: String) -> Self {
        let (tx, incoming) = mpsc::sync_channel(256);
        let (outgoing, rx) = mpsc::sync_channel(8);
        std::thread::spawn(move || run_worker(host, port, username, version, tx, rx));
        Self::with_channels(incoming, outgoing)
    }

    fn with_channels(
        incoming: Receiver<WorkerMessage>,
        outgoing: SyncSender<WorkerCommand>,
    ) -> Self {
        Self {
            incoming,
            outgoing,
            status: SessionStatus::Connecting,
            world: WorldState::new(DimensionMeta::default(), BlockTable::load(None)),
            hud: HudState::default(),
            magic: MagicHudState::default(),
            vfx: ClientVfxHost::new(GpuPipelineKind::Classic),
            motion: None,
            pose: None,
            camera_sync: None,
            terrain_dirty: false,
            vfx_admitted: 0,
            vfx_rejected: 0,
            animation_packets: 0,
            animation_rejected: 0,
            motion_packets: 0,
            chat_message: None,
            last_vfx_tick: Instant::now(),
            last_animation_tick: Instant::now(),
        }
    }

    /// Limit main-thread packet work per frame. No reads, writes or DNS on this thread.
    pub fn poll(&mut self, camera_position: [f32; 3]) {
        for _ in 0..32 {
            match self.incoming.try_recv() {
                Ok(WorkerMessage::Connected) => self.status = SessionStatus::Playing,
                Ok(WorkerMessage::Play(event)) => {
                    if let Err(error) = self.apply_event(event) {
                        self.status = SessionStatus::Disconnected(error);
                        break;
                    }
                }
                Ok(WorkerMessage::Closed(reason)) => {
                    self.status = SessionStatus::Disconnected(reason);
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !matches!(self.status, SessionStatus::Disconnected(_)) {
                        self.status = SessionStatus::Disconnected("network worker exited".into());
                    }
                    break;
                }
            }
        }
        // Preserve the last received terrain even if the peer closes immediately
        // after sending chunk data.
        let had_pending = self.world.pending_mesh_sections() > 0;
        match self.world.rebuild_dirty_bounded(24, camera_position) {
            Ok(rebuilt) => self.terrain_dirty |= rebuilt != 0 || had_pending,
            Err(error) if !matches!(self.status, SessionStatus::Disconnected(_)) => {
                self.status = SessionStatus::Disconnected(format!("mesh: {error}"));
            }
            Err(_) => {}
        }
        if !matches!(self.status, SessionStatus::Disconnected(_)) {
            let now = Instant::now();
            let elapsed_ticks = (now.duration_since(self.last_vfx_tick).as_millis() / 50).min(4);
            for _ in 0..elapsed_ticks {
                self.vfx.tick();
            }
            self.last_vfx_tick = now;
            let animation_dt = now.duration_since(self.last_animation_tick).as_secs_f32();
            self.last_animation_tick = now;
            if let Err(error) = self.world.advance_animations(animation_dt.min(0.1)) {
                self.status = SessionStatus::Disconnected(format!("animation: {error:?}"));
            }
        }
    }

    pub fn send_movement(&mut self, movement: PlayerMovement) {
        if !matches!(self.status, SessionStatus::Playing) || self.pose.is_none() {
            return;
        }
        self.pose = Some(movement.pose);
        let _ = self.outgoing.try_send(WorkerCommand::Movement(movement));
    }

    /// Apply a decoded packet, also used by framed protocol regression tests.
    pub fn apply_event(&mut self, event: ClientPlayEvent) -> Result<(), String> {
        match event {
            ClientPlayEvent::Play(event) => self.apply_play(event),
            ClientPlayEvent::CustomChannel(channel) => self.apply_custom(channel),
        }
    }

    fn apply_play(&mut self, event: PlayEvent) -> Result<(), String> {
        match event {
            PlayEvent::JoinGame { dimension, .. } | PlayEvent::Respawn { dimension } => {
                self.world.reset_dimension(dimension);
                self.terrain_dirty = true;
                self.pose = None;
            }
            PlayEvent::Teleport(teleport) => {
                let pose = PlayerPose::from_teleport(&teleport, self.pose);
                self.pose = Some(pose);
                self.camera_sync = Some(pose);
            }
            PlayEvent::ChunkData(chunk) => {
                self.world.apply_chunk(*chunk)?;
                self.terrain_dirty = true;
            }
            PlayEvent::UpdateLight { cx, cz, light } => {
                self.world.apply_light_update(cx, cz, &light)?;
            }
            PlayEvent::BlockUpdate { pos, state_id } => {
                self.world.apply_block_update(pos, state_id)?
            }
            PlayEvent::SectionBlocksUpdate(changes) => {
                for (pos, state_id) in changes {
                    self.world.apply_block_update(pos, state_id)?;
                }
            }
            PlayEvent::UnloadChunk { cx, cz } => {
                self.world.unload_chunk(cx, cz);
                self.terrain_dirty = true;
            }
            PlayEvent::SetHealth { health, food, .. } => {
                self.hud.health = health;
                self.hud.hunger = food.clamp(0, 20) as u8;
            }
            PlayEvent::SystemChat { text, overlay } => {
                if overlay {
                    self.hud.actionbar = text.clone();
                } else {
                    self.hud.chat_lines.push(text.clone());
                    if self.hud.chat_lines.len() > 100 {
                        self.hud.chat_lines.remove(0);
                    }
                }
                self.chat_message = Some(text);
            }
            PlayEvent::Disconnect(reason) => self.status = SessionStatus::Disconnected(reason),
            _ => {}
        }
        Ok(())
    }

    fn apply_custom(&mut self, channel: ClientCustomChannelEvent) -> Result<(), String> {
        match channel {
            ClientCustomChannelEvent::Magic(sync) => self.magic = magic_hud_from_sync(&sync),
            ClientCustomChannelEvent::Vfx(payload) => {
                match self.vfx.ingest_vfx1_payload(&payload) {
                    Ok(admitted) => self.vfx_admitted += admitted as u64,
                    Err(_) => self.vfx_rejected += 1,
                }
            }
            ClientCustomChannelEvent::Anim(payload) => {
                self.animation_packets += 1;
                if self.world.ingest_anm1_payload(&payload).is_err() {
                    self.animation_rejected += 1;
                }
            }
            ClientCustomChannelEvent::Motion(trigger) => {
                self.motion_packets += 1;
                self.motion = Some(trigger);
            }
            ClientCustomChannelEvent::Unknown { .. } => {}
        }
        Ok(())
    }
}

pub fn magic_hud_from_sync(sync: &DecodedMagicSync) -> MagicHudState {
    MagicHudState {
        mana: sync.mana,
        max_mana: sync.max_mana,
        active_cast: sync.cast.as_ref().map(|cast| {
            let (elapsed, total, channeled) = match cast.state {
                DecodedMagicCastState::CastTime {
                    total_ticks,
                    elapsed_ticks,
                } => (elapsed_ticks, total_ticks, false),
                DecodedMagicCastState::Channeled {
                    max_ticks,
                    elapsed_ticks,
                    ..
                } => (elapsed_ticks, max_ticks, true),
            };
            CastBarState {
                spell_label: cast.spell_id.clone(),
                progress: if total == 0 {
                    0.0
                } else {
                    elapsed as f32 / total as f32
                },
                channeled,
                interrupted_flash_ticks: 0,
            }
        }),
        cooldowns: sync
            .cooldowns
            .iter()
            .map(|(label, &ticks)| CooldownSlotState {
                spell_label: label.clone(),
                remaining_ticks: ticks,
                total_ticks: ticks,
            })
            .collect(),
        custom_statuses: sync
            .statuses
            .iter()
            .map(|status| StatusBadgeState {
                label: status.id.clone(),
                remaining_ticks: status.remaining_ticks,
                amplifier: status.amplifier,
                is_harmful: false,
                shield_amount: (status.absorption_remaining > 0.0)
                    .then_some(status.absorption_remaining),
            })
            .collect(),
    }
}

fn run_worker(
    host: String,
    port: u16,
    username: String,
    version: String,
    tx: SyncSender<WorkerMessage>,
    rx: Receiver<WorkerCommand>,
) {
    let result = run_connection(&host, port, &username, &version, &tx, rx);
    if let Err(error) = result {
        let _ = tx.send(WorkerMessage::Closed(error));
    }
}

fn run_connection(
    host: &str,
    port: u16,
    username: &str,
    version: &str,
    tx: &SyncSender<WorkerMessage>,
    rx: Receiver<WorkerCommand>,
) -> Result<(), String> {
    let protocol = mc_spine_client::cli::parse_mc_version(version)?;
    let (mut client, _) = ProtocolClient::connect_for_version(
        host,
        port,
        username,
        offline_uuid(username),
        10,
        Duration::from_secs(8),
        protocol,
    )?;
    if tx.send(WorkerMessage::Connected).is_err() {
        return Ok(());
    }
    let mut last_input = None;
    let mut last_send = Instant::now();
    loop {
        loop {
            match rx.try_recv() {
                Ok(WorkerCommand::Movement(movement)) => last_input = Some(movement),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        if last_send.elapsed() >= Duration::from_millis(50) {
            if let Some(input) = last_input {
                let pose = input.pose;
                client.send_player_position_rotation(
                    pose.feet[0],
                    pose.feet[1],
                    pose.feet[2],
                    pose.yaw,
                    pose.pitch,
                    false,
                )?;
                client.send_player_input(input.strafe, input.forward, input.jump, false)?;
            }
            last_send = Instant::now();
        }
        match client.recv_play_with_custom_channels(Duration::from_millis(20))? {
            Some(ClientPlayEvent::Play(PlayEvent::KeepAlive(id))) => client.send_keep_alive(id)?,
            Some(ClientPlayEvent::Play(PlayEvent::ChunkBatchFinished(count))) => {
                client.send_chunk_batch_received((count.max(1)) as f32)?;
            }
            Some(ClientPlayEvent::Play(PlayEvent::Teleport(ref teleport))) => {
                let previous_pose = last_input.map(|movement: PlayerMovement| movement.pose);
                let pose = PlayerPose::from_teleport(teleport, previous_pose);
                client.send_accept_teleportation(
                    teleport.teleport_id,
                    pose.feet,
                    pose.yaw,
                    pose.pitch,
                    false,
                )?;
                if tx
                    .send(WorkerMessage::Play(ClientPlayEvent::Play(
                        PlayEvent::Teleport(teleport.clone()),
                    )))
                    .is_err()
                {
                    return Ok(());
                }
                last_input = Some(PlayerMovement {
                    pose,
                    strafe: 0.0,
                    forward: 0.0,
                    jump: false,
                });
            }
            Some(ClientPlayEvent::Play(PlayEvent::Disconnect(reason))) => return Err(reason),
            Some(event) => {
                if tx.send(WorkerMessage::Play(event)).is_err() {
                    return Ok(());
                }
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::{
        probe_adapters, request_gpu_device, section_geometry, GpuRenderer, RendererError,
        OFFSCREEN_FORMAT,
    };
    use crate::Camera;
    use mc_spine_client::protocol::{
        decode_chunk_packet, decode_custom_payload_packet, decode_packet_body, encode_packet,
        DecodedMotionPhase, WireReader, CHANNEL_ANIM_V1, CHANNEL_MAGIC_V1, CHANNEL_MOTION_V1,
        CHANNEL_VFX_V1,
    };
    use mc_spine_client_mesh::vfx_engine::{DynamicColoredLight, Vfx1Packet, VfxCommand};
    use mc_spine_client_mesh::RenderBackendPreference;
    use mc_spine_kernel::skeletal_anim::{encode_animation_trigger, AnimationTrigger, LoopMode};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    fn session() -> (LiveSession, SyncSender<WorkerMessage>) {
        let (tx, incoming) = mpsc::sync_channel(8);
        let (outgoing, _rx) = mpsc::sync_channel(8);
        (LiveSession::with_channels(incoming, outgoing), tx)
    }

    fn varint(out: &mut Vec<u8>, mut value: u32) {
        loop {
            let part = (value & 0x7f) as u8;
            value >>= 7;
            out.push(part | if value == 0 { 0 } else { 0x80 });
            if value == 0 {
                break;
            }
        }
    }

    fn wire_string(out: &mut Vec<u8>, value: &str) {
        varint(out, value.len() as u32);
        out.extend_from_slice(value.as_bytes());
    }

    fn decoded_frame(packet_id: i32, payload: &[u8]) -> Vec<u8> {
        // Exercise the same VarInt length/pid framing as the TCP receiver.
        let frame = encode_packet(packet_id, payload, None);
        let mut reader = WireReader::new(&frame);
        let length = usize::try_from(reader.read_varint().unwrap()).unwrap();
        assert_eq!(reader.remaining(), length);
        let (decoded_id, body) = decode_packet_body(reader.rest(), None).unwrap();
        assert_eq!(decoded_id, packet_id);
        body
    }

    fn read_loopback_frame(socket: &mut TcpStream) -> (i32, Vec<u8>) {
        let mut length = 0u32;
        for shift in (0..35).step_by(7) {
            let mut byte = [0];
            socket.read_exact(&mut byte).unwrap();
            length |= u32::from(byte[0] & 0x7f) << shift;
            if byte[0] & 0x80 == 0 {
                break;
            }
        }
        assert!(length < 8 * 1024 * 1024);
        let mut packet = vec![0; length as usize];
        socket.read_exact(&mut packet).unwrap();
        decode_packet_body(&packet, None).unwrap()
    }

    fn send_loopback_frame(socket: &mut TcpStream, packet_id: i32, data: &[u8]) {
        socket
            .write_all(&encode_packet(packet_id, data, None))
            .unwrap();
    }

    fn framed(channel: &str, payload: &[u8]) -> ClientPlayEvent {
        let mut wire = Vec::new();
        wire_string(&mut wire, channel);
        wire.extend_from_slice(payload);
        let body = decoded_frame(0x19, &wire);
        ClientPlayEvent::CustomChannel(
            decode_custom_payload_packet(&body).expect("framed custom packet"),
        )
    }

    fn stone_chunk() -> Vec<u8> {
        // Protocol 767 level_chunk_with_light body, dimension -64..320.
        // A single 16x16x16 solid stone section is surrounded by air.
        let mut sections = Vec::new();
        for section in 0..24 {
            sections.extend_from_slice(&(if section == 4 { 4096i16 } else { 0 }).to_be_bytes());
            sections.push(0); // single-valued block palette
            varint(&mut sections, if section == 4 { 1 } else { 0 });
            varint(&mut sections, 0); // no block data array
            sections.push(0); // single-valued biome palette
            varint(&mut sections, 0);
            varint(&mut sections, 0);
        }
        let mut body = Vec::new();
        body.extend_from_slice(&0i32.to_be_bytes()); // chunk X
        body.extend_from_slice(&0i32.to_be_bytes()); // chunk Z
        body.extend_from_slice(&[10, 0]); // empty unnamed compound heightmap
        varint(&mut body, sections.len() as u32);
        body.extend_from_slice(&sections);
        for _ in 0..5 {
            varint(&mut body, 0);
        } // block entities and four light masks
        varint(&mut body, 0); // sky arrays
        varint(&mut body, 0); // block arrays
        body
    }

    fn assert_live_gpu_frame(world: &WorldState) -> Option<GpuRenderer> {
        let reports = probe_adapters(RenderBackendPreference::Auto);
        if reports.is_empty() {
            eprintln!("SKIP live GPU render: probe_adapters found 0 adapters");
            return None;
        }
        let ctx = match request_gpu_device(RenderBackendPreference::Auto, None, None) {
            Ok(ctx) => ctx,
            Err(RendererError::NoAdapter(error)) => {
                panic!("probe found adapters but request failed: {error}")
            }
            Err(error) => panic!("live GPU setup failed: {error}"),
        };
        eprintln!(
            "live GPU test adapter: {:?} {}",
            ctx.report.backend, ctx.report.name
        );
        let mut renderer = GpuRenderer::new(ctx, OFFSCREEN_FORMAT);
        let camera = Camera {
            position: [8.0, 20.0, 30.0],
            yaw_deg: 0.0,
            pitch_deg: -25.0,
            ..Camera::default()
        };
        let blank = renderer
            .render_offscreen_rgba(128, 128, &camera, None)
            .unwrap();
        assert_eq!(renderer.update_live_terrain(world), 1);
        assert_eq!(renderer.live_section_count(), 1);
        let rendered = renderer
            .render_offscreen_rgba(128, 128, &camera, None)
            .unwrap();
        assert!(
            rendered
                .iter()
                .zip(&blank)
                .any(|(pixel, baseline)| pixel != baseline),
            "decoded live chunk must change GPU pixels from an empty world"
        );
        eprintln!(
            "live chunk changed offscreen GPU frame ({} bytes)",
            rendered.len()
        );
        Some(renderer)
    }

    #[test]
    fn loopback_tcp_767_worker_receives_chunks_and_channels() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (done_tx, done_rx) = mpsc::channel::<()>();

        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            assert_eq!(read_loopback_frame(&mut socket).0, 0); // handshake
            assert_eq!(read_loopback_frame(&mut socket).0, 0); // login_start

            let mut login = offline_uuid("Loopback").to_vec();
            wire_string(&mut login, "Loopback");
            varint(&mut login, 0); // profile properties
            login.push(0);
            send_loopback_frame(&mut socket, 0x02, &login);
            assert_eq!(read_loopback_frame(&mut socket).0, 0x03); // login_acknowledged

            send_loopback_frame(&mut socket, 0x03, &[]); // finish_configuration
            assert_eq!(read_loopback_frame(&mut socket).0, 0x00); // client_information
            assert_eq!(read_loopback_frame(&mut socket).0, 0x03); // finish ack

            send_loopback_frame(&mut socket, 0x26, &29i64.to_be_bytes());
            let mut teleport = Vec::new();
            for value in [8.0f64, 64.0, 30.0] {
                teleport.extend_from_slice(&value.to_be_bytes());
            }
            teleport.extend_from_slice(&0.0f32.to_be_bytes()); // yaw
            teleport.extend_from_slice(&0.0f32.to_be_bytes()); // pitch
            teleport.push(0); // absolute position
            varint(&mut teleport, 3); // teleport ID
            send_loopback_frame(&mut socket, 0x40, &teleport);
            send_loopback_frame(&mut socket, 0x27, &stone_chunk());

            let mut magic = b"MAG1".to_vec();
            magic.extend_from_slice(&9.0f32.to_le_bytes());
            magic.extend_from_slice(&20.0f32.to_le_bytes());
            magic.push(0); // no cast
            varint(&mut magic, 0); // no cooldowns
            varint(&mut magic, 0); // no statuses
            let mut payload = Vec::new();
            wire_string(&mut payload, CHANNEL_MAGIC_V1);
            payload.extend_from_slice(&magic);
            send_loopback_frame(&mut socket, 0x19, &payload);

            let vfx = Vfx1Packet {
                commands: vec![VfxCommand::SpawnLight {
                    id: 17,
                    light: DynamicColoredLight {
                        pos: [4.0, 4.0, 4.0],
                        rgb: [1.0, 1.0, 1.0],
                        radius: 3.0,
                        intensity: 1.0,
                        ttl_ticks: 50,
                        priority: 1,
                    },
                }],
            }
            .encode()
            .unwrap();
            payload.clear();
            wire_string(&mut payload, CHANNEL_VFX_V1);
            payload.extend_from_slice(&vfx);
            send_loopback_frame(&mut socket, 0x19, &payload);

            let animation = encode_animation_trigger(AnimationTrigger {
                entity_id: 77,
                layer: 0,
                clip_id: 0,
                speed: 1.0,
                fade_ticks: 0,
                loop_mode: LoopMode::Loop,
                restart: true,
            })
            .unwrap();
            payload.clear();
            wire_string(&mut payload, CHANNEL_ANIM_V1);
            payload.extend_from_slice(&animation);
            send_loopback_frame(&mut socket, 0x19, &payload);

            let mut keep_alive = false;
            let mut accepted_teleport = false;
            for _ in 0..8 {
                let (packet_id, body) = read_loopback_frame(&mut socket);
                if packet_id == 0x18 {
                    assert_eq!(body, 29i64.to_be_bytes());
                    keep_alive = true;
                }
                if packet_id == 0x00 {
                    assert_eq!(WireReader::new(&body).read_varint().unwrap(), 3);
                    accepted_teleport = true;
                }
                if keep_alive && accepted_teleport {
                    break;
                }
            }
            assert!(
                keep_alive && accepted_teleport,
                "server must receive play acknowledgments"
            );
            done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        });

        let mut live =
            LiveSession::connect("127.0.0.1".into(), port, "Loopback".into(), "767".into());
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            live.poll([8.0, 20.0, 30.0]);
            if live.world.loaded_chunks() == 1
                && live.magic.mana == 9.0
                && live.vfx_admitted == 1
                && live.animation_packets == 1
                && live.camera_sync.is_some()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            live.world.loaded_chunks(),
            1,
            "TCP chunk should reach ClientWorld"
        );
        assert_eq!(live.magic.mana, 9.0, "TCP MAG1 should reach the HUD");
        assert_eq!(live.vfx_admitted, 1, "TCP VFX1 should reach the VFX host");
        assert_eq!(
            live.animation_packets, 1,
            "TCP ANM1 should reach animation host"
        );
        assert_eq!(live.camera_sync.unwrap().feet, [8.0, 64.0, 30.0]);
        done_tx.send(()).unwrap();
        server.join().unwrap();
        let mesh = live.world.packed_meshes().next().expect("TCP chunk mesh");
        let geometry =
            section_geometry(mesh.1, live.world.translucent_order(mesh.0).unwrap_or(&[]));
        assert!(!geometry.opaque.is_empty());
        let _ = assert_live_gpu_frame(&live.world);
    }

    #[test]
    fn state_and_relative_teleport() {
        let (_tx, incoming) = mpsc::sync_channel(1);
        let (outgoing, _rx) = mpsc::sync_channel(1);
        let mut session = LiveSession::with_channels(incoming, outgoing);
        assert_eq!(session.status, SessionStatus::Connecting);
        session.status = SessionStatus::Playing;
        let teleport = TeleportSync {
            x: 2.0,
            y: 3.0,
            z: 4.0,
            yaw: 15.0,
            pitch: -5.0,
            flags: 0x1f,
            teleport_id: 7,
        };
        let base = PlayerPose {
            feet: [4.0, 5.0, 6.0],
            yaw: 90.0,
            pitch: 10.0,
        };
        let pose = PlayerPose::from_teleport(&teleport, Some(base));
        assert_eq!(pose.feet, [6.0, 8.0, 10.0]);
        assert_eq!((pose.yaw, pose.pitch), (105.0, 5.0));
        session.pose = Some(base);
        session
            .apply_event(ClientPlayEvent::Play(PlayEvent::Teleport(teleport)))
            .unwrap();
        assert_eq!(session.camera_sync, Some(pose));
    }

    #[test]
    fn worker_messages_and_hud_play_events() {
        let (mut live, tx) = session();
        tx.send(WorkerMessage::Connected).unwrap();
        tx.send(WorkerMessage::Play(ClientPlayEvent::Play(
            PlayEvent::SetHealth {
                health: 14.0,
                food: 12,
                saturation: 2.0,
            },
        )))
        .unwrap();
        tx.send(WorkerMessage::Play(ClientPlayEvent::Play(
            PlayEvent::SystemChat {
                text: "server ready".into(),
                overlay: false,
            },
        )))
        .unwrap();
        live.poll([0.0; 3]);
        assert_eq!(live.status, SessionStatus::Playing);
        assert_eq!(live.hud.health, 14.0);
        assert_eq!(live.hud.hunger, 12);
        assert_eq!(live.hud.chat_lines.last().unwrap(), "server ready");
        tx.send(WorkerMessage::Closed("test disconnect".into()))
            .unwrap();
        live.poll([0.0; 3]);
        assert_eq!(
            live.status,
            SessionStatus::Disconnected("test disconnect".into())
        );
    }

    #[test]
    fn framed_magic_vfx_anim_motion_reach_live_hosts() {
        let (mut live, _tx) = session();
        let mut magic = b"MAG1".to_vec();
        magic.extend_from_slice(&7.5f32.to_le_bytes());
        magic.extend_from_slice(&20f32.to_le_bytes());
        magic.push(1); // cast time
        wire_string(&mut magic, "spell:flame");
        varint(&mut magic, 100);
        varint(&mut magic, 25);
        varint(&mut magic, 1); // cooldowns
        wire_string(&mut magic, "spell:flame");
        varint(&mut magic, 40);
        varint(&mut magic, 0); // statuses
        live.apply_event(framed(CHANNEL_MAGIC_V1, &magic)).unwrap();
        assert_eq!((live.magic.mana, live.magic.max_mana), (7.5, 20.0));
        assert_eq!(live.magic.active_cast.as_ref().unwrap().progress, 0.25);
        assert_eq!(live.magic.cooldowns[0].remaining_ticks, 40);
        assert!(!live.magic.is_empty());

        let vfx = Vfx1Packet {
            commands: vec![VfxCommand::SpawnLight {
                id: 42,
                light: DynamicColoredLight {
                    pos: [3.0, 4.0, 5.0],
                    rgb: [1.0, 0.5, 0.25],
                    radius: 6.0,
                    intensity: 2.0,
                    ttl_ticks: 20,
                    priority: 2,
                },
            }],
        }
        .encode()
        .unwrap();
        live.apply_event(framed(CHANNEL_VFX_V1, &vfx)).unwrap();
        assert_eq!(live.vfx_admitted, 1);
        assert_eq!(live.vfx.frame_output([0.0, 0.0, 1.0]).lights.len(), 1);

        const GEOMETRY: &str = r#"{"format_version":"1.12.0","minecraft:geometry":[{"description":{"identifier":"geometry.test"},"bones":[{"name":"root","pivot":[0,0,0]}]}]}"#;
        const ANIMATIONS: &str = r#"{"format_version":"1.8.0","animations":{"animation.test.move":{"loop":true,"animation_length":1.0,"bones":{"root":{"position":{"0.0":[0,0,0],"1.0":[2,0,0]}}}}}}"#;
        live.world
            .animations
            .register_model_json("test", GEOMETRY, ANIMATIONS)
            .unwrap();
        live.world.animations.bind_entity(77, "test").unwrap();
        let before = live.world.evaluate_entity_pose(77).unwrap().unwrap();
        let trigger = encode_animation_trigger(AnimationTrigger {
            entity_id: 77,
            layer: 0,
            clip_id: live
                .world
                .animations
                .clip_id("test", "animation.test.move")
                .unwrap(),
            speed: 1.0,
            fade_ticks: 0,
            loop_mode: LoopMode::Loop,
            restart: true,
        })
        .unwrap();
        live.apply_event(framed(CHANNEL_ANIM_V1, &trigger)).unwrap();
        assert_eq!(live.animation_packets, 1);
        assert_eq!(live.animation_rejected, 0);
        live.world.advance_animations(0.5).unwrap();
        assert_ne!(
            live.world.evaluate_entity_pose(77).unwrap().unwrap(),
            before
        );

        let mut motion = b"MOT1".to_vec();
        motion.extend_from_slice(&77u64.to_be_bytes());
        motion.extend_from_slice(&123u32.to_be_bytes());
        motion.extend_from_slice(&2u16.to_be_bytes());
        motion.push(0); // main hand
        motion.extend_from_slice(&1.0f32.to_be_bytes());
        motion.extend_from_slice(&5u32.to_be_bytes());
        motion.push(2); // active hit window
        live.apply_event(framed(CHANNEL_MOTION_V1, &motion))
            .unwrap();
        assert_eq!(live.motion_packets, 1);
        assert_eq!(
            live.motion.unwrap().phase,
            DecodedMotionPhase::ActiveHitWindow
        );
    }

    #[test]
    fn decoded_play_chunk_meshes_and_renders_live_gpu_frame() {
        let (mut live, _tx) = session();
        let chunk_body = decoded_frame(0x27, &stone_chunk());
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ChunkData(Box::new(
            decode_chunk_packet(&chunk_body, 24, 64).unwrap(),
        ))))
        .unwrap();
        live.poll([8.0, 8.0, 24.0]);
        assert_eq!(live.world.loaded_chunks(), 1);
        let mesh = live
            .world
            .packed_meshes()
            .next()
            .expect("solid section mesh");
        let geometry =
            section_geometry(mesh.1, live.world.translucent_order(mesh.0).unwrap_or(&[]));
        assert!(!geometry.opaque.is_empty());
        assert!(!geometry.opaque_indices.is_empty());

        let renderer = assert_live_gpu_frame(&live.world);

        live.apply_event(ClientPlayEvent::Play(PlayEvent::UnloadChunk {
            cx: 0,
            cz: 0,
        }))
        .unwrap();
        assert_eq!(live.world.loaded_chunks(), 0);
        if let Some(mut renderer) = renderer {
            renderer.update_live_terrain(&live.world);
            assert_eq!(renderer.live_section_count(), 0);
        }
    }
}
