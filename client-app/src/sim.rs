// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Client-side 20 Hz physics prediction (`client-kernel`), scripted movement loop,
//! and headless Protocol 767 session runner.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use mc_spine_kernel::{collide, fluid_impulse, Aabb, FluidKind, FluidQuery};

use crate::cli::{CliConfig, ScriptMode};
use crate::lod::LodManager;
use crate::protocol::{offline_uuid, parse_uuid, PlayEvent, ProtocolClient, TeleportSync};
use crate::world::{BlockTable, WorldState};

pub const PLAYER_HALF_WIDTH: f64 = 0.3;
pub const PLAYER_HEIGHT: f64 = 1.8;
pub const PLAYER_EYE_HEIGHT: f64 = 1.62;
pub const PLAYER_MAX_UP_STEP: f32 = 0.6;
pub const GRAVITY: f64 = 0.08;
pub const AIR_DRAG_Y: f64 = 0.98;
pub const WATER_VERTICAL_DRAG: f64 = 0.800000011920929;
pub const JUMP_VELOCITY: f64 = 0.42;

#[derive(Clone, Copy, Debug, Default)]
pub struct TickInput {
    pub forward: f64,
    pub strafe: f64,
    pub jump: bool,
    pub yaw_delta: f32,
    pub pitch_delta: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct StepOutcome {
    pub pos: [f64; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub on_ground: bool,
    pub in_fluid: bool,
    pub horizontal_collision: bool,
    pub stepped_up: bool,
}

#[derive(Clone, Debug)]
pub struct PlayerPhysics {
    pub pos: [f64; 3],
    pub vel: [f64; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub on_ground: bool,
    pub in_water: bool,
    pub in_lava: bool,
}

impl PlayerPhysics {
    pub fn new(pos: [f64; 3], yaw: f32, pitch: f32) -> Self {
        Self {
            pos,
            vel: [0.0; 3],
            yaw,
            pitch,
            on_ground: false,
            in_water: false,
            in_lava: false,
        }
    }

    pub fn aabb(&self) -> Aabb {
        Aabb {
            min: [
                self.pos[0] - PLAYER_HALF_WIDTH,
                self.pos[1],
                self.pos[2] - PLAYER_HALF_WIDTH,
            ],
            max: [
                self.pos[0] + PLAYER_HALF_WIDTH,
                self.pos[1] + PLAYER_HEIGHT,
                self.pos[2] + PLAYER_HALF_WIDTH,
            ],
        }
    }

    pub fn camera(&self) -> [f32; 3] {
        [
            self.pos[0] as f32,
            (self.pos[1] + PLAYER_EYE_HEIGHT) as f32,
            self.pos[2] as f32,
        ]
    }

    pub fn apply_teleport(&mut self, tp: &TeleportSync) -> f64 {
        let prev = self.pos;
        if tp.flags & 0x01 != 0 {
            self.pos[0] += tp.x;
        } else {
            self.pos[0] = tp.x;
        }
        if tp.flags & 0x02 != 0 {
            self.pos[1] += tp.y;
        } else {
            self.pos[1] = tp.y;
        }
        if tp.flags & 0x04 != 0 {
            self.pos[2] += tp.z;
        } else {
            self.pos[2] = tp.z;
        }
        if tp.flags & 0x08 != 0 {
            self.yaw += tp.yaw;
        } else {
            self.yaw = tp.yaw;
        }
        if tp.flags & 0x10 != 0 {
            self.pitch += tp.pitch;
        } else {
            self.pitch = tp.pitch;
        }
        self.vel = [0.0; 3];
        let dx = self.pos[0] - prev[0];
        let dy = self.pos[1] - prev[1];
        let dz = self.pos[2] - prev[2];
        (dx * dx + dy * dy + dz * dz).sqrt()
    }

    pub fn step(&mut self, world: &WorldState, input: TickInput) -> Result<StepOutcome, String> {
        self.yaw = (self.yaw + input.yaw_delta) % 360.0;
        self.pitch = (self.pitch + input.pitch_delta).clamp(-89.0, 89.0);

        let bx = self.pos[0].floor() as i32;
        let bz = self.pos[2].floor() as i32;
        if !world
            .columns
            .contains_key(&(bx.div_euclid(16), bz.div_euclid(16)))
        {
            return Ok(StepOutcome {
                pos: self.pos,
                yaw: self.yaw,
                pitch: self.pitch,
                on_ground: self.on_ground,
                in_fluid: false,
                horizontal_collision: false,
                stepped_up: false,
            });
        }

        let box_now = self.aabb();
        let water_imp = fluid_impulse(
            box_now,
            FluidQuery {
                kind: FluidKind::Water,
                ultra_warm: false,
                pushed_by_fluid: true,
                is_player: true,
                velocity: self.vel,
            },
            |x, y, z| Ok(world.fluid_cell_at(x, y, z)),
        )
        .map_err(|e| format!("fluid_impulse(water) failed: {e:?}"))?;

        let lava_imp = fluid_impulse(
            box_now,
            FluidQuery {
                kind: FluidKind::Lava,
                ultra_warm: world.dimension.name == "minecraft:the_nether",
                pushed_by_fluid: true,
                is_player: true,
                velocity: self.vel,
            },
            |x, y, z| Ok(world.fluid_cell_at(x, y, z)),
        )
        .map_err(|e| format!("fluid_impulse(lava) failed: {e:?}"))?;

        self.in_water = water_imp.in_fluid;
        self.in_lava = lava_imp.in_fluid;
        for axis in 0..3 {
            self.vel[axis] += water_imp.push[axis] + lava_imp.push[axis];
        }

        let yaw_rad = f64::from(self.yaw).to_radians();
        let forward_x = -yaw_rad.sin();
        let forward_z = yaw_rad.cos();
        let right_x = -forward_z;
        let right_z = forward_x;

        let in_fluid = self.in_water || self.in_lava;
        let horiz_scale = if in_fluid { 0.45 } else { 1.0 };
        let dx = (forward_x * input.forward + right_x * input.strafe) * horiz_scale;
        let dz = (forward_z * input.forward + right_z * input.strafe) * horiz_scale;

        let vy = if in_fluid {
            if input.jump {
                self.vel[1] + 0.04
            } else {
                self.vel[1]
            }
        } else if self.on_ground && input.jump {
            JUMP_VELOCITY
        } else {
            (self.vel[1] - GRAVITY) * AIR_DRAG_Y
        };

        let delta = [dx, vy, dz];
        let sweep = Aabb {
            min: [
                box_now.min[0] + delta[0].min(0.0),
                box_now.min[1] + delta[1].min(0.0),
                box_now.min[2] + delta[2].min(0.0),
            ],
            max: [
                box_now.max[0] + delta[0].max(0.0),
                box_now.max[1] + delta[1].max(0.0) + f64::from(PLAYER_MAX_UP_STEP),
                box_now.max[2] + delta[2].max(0.0),
            ],
        };
        let shapes = world.collect_collision_aabbs(sweep);
        let col = collide(box_now, delta, self.on_ground, PLAYER_MAX_UP_STEP, &shapes)
            .map_err(|e| format!("collide failed: {e:?}"))?;

        self.pos[0] += col.delta[0];
        self.pos[1] += col.delta[1];
        self.pos[2] += col.delta[2];
        self.on_ground = col.vertical_collision_below;

        self.vel[0] = if col.horizontal_collision {
            0.0
        } else {
            col.delta[0] * 0.546
        };
        self.vel[2] = if col.horizontal_collision {
            0.0
        } else {
            col.delta[2] * 0.546
        };
        self.vel[1] = if col.vertical_collision {
            0.0
        } else if self.in_water {
            col.delta[1] * WATER_VERTICAL_DRAG - GRAVITY / 16.0
        } else if self.in_lava {
            col.delta[1] * 0.5 - GRAVITY / 4.0
        } else {
            vy
        };

        Ok(StepOutcome {
            pos: self.pos,
            yaw: self.yaw,
            pitch: self.pitch,
            on_ground: self.on_ground,
            in_fluid,
            horizontal_collision: col.horizontal_collision,
            stepped_up: col.stepped_up,
        })
    }
}

pub fn script_input(mode: ScriptMode, tick: u32, on_ground: bool) -> TickInput {
    match mode {
        ScriptMode::Idle => TickInput::default(),
        ScriptMode::Walk => TickInput {
            forward: 0.16,
            ..TickInput::default()
        },
        ScriptMode::Sprint => TickInput {
            forward: 0.25,
            ..TickInput::default()
        },
        ScriptMode::Jump => TickInput {
            forward: 0.10,
            jump: on_ground && tick.is_multiple_of(12),
            ..TickInput::default()
        },
        ScriptMode::Survey => TickInput {
            forward: if tick.is_multiple_of(4) { 0.08 } else { 0.0 },
            yaw_delta: 9.0,
            pitch_delta: if (tick / 10).is_multiple_of(2) {
                1.5
            } else {
                -1.5
            },
            ..TickInput::default()
        },
    }
}

#[derive(Clone, Debug)]
pub struct SessionSummary {
    pub username: String,
    pub server: String,
    /// Protocol version negotiated for this session (`767` or `777`).
    pub protocol: i32,
    pub login_ok: bool,
    pub config_ok: bool,
    pub play_ok: bool,
    pub compression_threshold: Option<usize>,
    pub dimension: String,
    pub entity_id: i32,
    pub script: ScriptMode,
    pub ticks_executed: u32,
    pub chunks_decoded: u64,
    pub chunks_loaded: usize,
    pub chunks_unloaded: u64,
    pub chunk_batches: u64,
    pub light_updates: u64,
    pub block_updates: u64,
    pub local_light_edits: u64,
    pub sections_rebuilt: u64,
    pub total_quads: usize,
    pub packed_vertices: usize,
    pub translucent_quads: usize,
    pub avg_mesh_us_per_section: f64,
    pub total_mesh_us: u64,
    pub total_pack_us: u64,
    pub total_sort_us: u64,
    pub block_states_from_json: bool,
    pub teleports_0x40: u64,
    pub corrections_0x40: u64,
    pub max_prediction_drift: f64,
    pub final_pos: [f64; 3],
    pub on_ground: bool,
    pub health: f32,
    pub food: i32,
    pub keep_alives: u64,
    pub packets_rx: u64,
    pub packets_tx: u64,
    /// Effective view distance used for the session (explicit flag or the
    /// resolved `ClientSettings` value).
    pub view_distance: i8,
    /// LOD columns stored by the far-terrain `LodManager` (0 when
    /// `distant_horizon` is off).
    pub lod_stored_columns: usize,
    /// Columns drawn at fine LOD in the final frame set.
    pub lod_fine_drawn: usize,
    /// Columns drawn at coarse LOD in the final frame set.
    pub lod_coarse_drawn: usize,
    pub render_ms: f64,
    pub triangles_drawn: u64,
    pub pixels_shaded: u64,
    pub drawn_sections: u64,
    pub culled_sections: u64,
    pub region_draw_calls: u64,
    pub buckets_drawn: u64,
    pub buckets_culled: u64,
}

impl SessionSummary {
    pub fn to_json(&self) -> String {
        let comp = match self.compression_threshold {
            Some(t) => t.to_string(),
            None => "null".to_string(),
        };
        format!(
            concat!(
                "{{",
                "\"status\":\"ok\",",
                "\"protocol\":{},",
                "\"username\":\"{}\",",
                "\"server\":\"{}\",",
                "\"login_ok\":{},",
                "\"config_ok\":{},",
                "\"play_ok\":{},",
                "\"compression_threshold\":{},",
                "\"dimension\":\"{}\",",
                "\"entity_id\":{},",
                "\"script\":\"{}\",",
                "\"ticks_executed\":{},",
                "\"chunks_decoded\":{},",
                "\"chunks_loaded\":{},",
                "\"chunks_unloaded\":{},",
                "\"chunk_batches\":{},",
                "\"light_updates\":{},",
                "\"block_updates\":{},",
                "\"local_light_edits\":{},",
                "\"sections_rebuilt\":{},",
                "\"total_quads\":{},",
                "\"packed_vertices\":{},",
                "\"translucent_quads\":{},",
                "\"avg_mesh_us_per_section\":{:.2},",
                "\"total_mesh_us\":{},",
                "\"total_pack_us\":{},",
                "\"total_sort_us\":{},",
                "\"block_states_from_json\":{},",
                "\"teleports_0x40\":{},",
                "\"corrections_0x40\":{},",
                "\"max_prediction_drift\":{:.6},",
                "\"final_pos\":[{:.4},{:.4},{:.4}],",
                "\"on_ground\":{},",
                "\"health\":{:.2},",
                "\"food\":{},",
                "\"keep_alives\":{},",
                "\"packets_rx\":{},",
                "\"packets_tx\":{},",
                "\"view_distance\":{},",
                "\"lod_stored_columns\":{},",
                "\"lod_fine_drawn\":{},",
                "\"lod_coarse_drawn\":{},",
                "\"render_ms\":{:.3},",
                "\"triangles_drawn\":{},",
                "\"pixels_shaded\":{},",
                "\"drawn_sections\":{},",
                "\"culled_sections\":{},",
                "\"region_draw_calls\":{},",
                "\"buckets_drawn\":{},",
                "\"buckets_culled\":{}",
                "}}"
            ),
            self.protocol,
            escape_json(&self.username),
            escape_json(&self.server),
            self.login_ok,
            self.config_ok,
            self.play_ok,
            comp,
            escape_json(&self.dimension),
            self.entity_id,
            self.script.as_str(),
            self.ticks_executed,
            self.chunks_decoded,
            self.chunks_loaded,
            self.chunks_unloaded,
            self.chunk_batches,
            self.light_updates,
            self.block_updates,
            self.local_light_edits,
            self.sections_rebuilt,
            self.total_quads,
            self.packed_vertices,
            self.translucent_quads,
            self.avg_mesh_us_per_section,
            self.total_mesh_us,
            self.total_pack_us,
            self.total_sort_us,
            self.block_states_from_json,
            self.teleports_0x40,
            self.corrections_0x40,
            self.max_prediction_drift,
            self.final_pos[0],
            self.final_pos[1],
            self.final_pos[2],
            self.on_ground,
            self.health,
            self.food,
            self.keep_alives,
            self.packets_rx,
            self.packets_tx,
            self.view_distance,
            self.lod_stored_columns,
            self.lod_fine_drawn,
            self.lod_coarse_drawn,
            self.render_ms,
            self.triangles_drawn,
            self.pixels_shaded,
            self.drawn_sections,
            self.culled_sections,
            self.region_draw_calls,
            self.buckets_drawn,
            self.buckets_culled,
        )
    }

    pub fn to_human(&self) -> String {
        format!(
            concat!(
                "mc-spine-client session complete (Protocol {})\n",
                "  server:        {} (dimension={}, entity_id={})\n",
                "  handshake:     login={} config={} play={} compression={:?}\n",
                "  world:         chunks_decoded={} loaded={} unloaded={} batches={} light_updates={} block_updates={} local_light={}\n",
                "  meshing:       sections_rebuilt={} quads={} packed_vertices={} translucent={} avg={:.1} us/section (block_json={})\n",
                "  render:        {:.2} ms, {} triangles, {} px shaded\n",
                "  culling:       sections drawn={} culled={} | region_draw_calls={} | buckets drawn={} culled={}\n",
                "  prediction:    script={} ticks={} pos=({:.3}, {:.3}, {:.3}) on_ground={} teleports={} corrections_0x40={} max_drift={:.6}\n",
                "  settings:      view_distance={} lod_stored={} lod_fine={} lod_coarse={}\n",
                "  network:       health={:.1} food={} keep_alives={} rx={} tx={}"
            ),
            self.protocol,
            self.server,
            self.dimension,
            self.entity_id,
            self.login_ok,
            self.config_ok,
            self.play_ok,
            self.compression_threshold,
            self.chunks_decoded,
            self.chunks_loaded,
            self.chunks_unloaded,
            self.chunk_batches,
            self.light_updates,
            self.block_updates,
            self.local_light_edits,
            self.sections_rebuilt,
            self.total_quads,
            self.packed_vertices,
            self.translucent_quads,
            self.avg_mesh_us_per_section,
            self.block_states_from_json,
            self.render_ms,
            self.triangles_drawn,
            self.pixels_shaded,
            self.drawn_sections,
            self.culled_sections,
            self.region_draw_calls,
            self.buckets_drawn,
            self.buckets_culled,
            self.script.as_str(),
            self.ticks_executed,
            self.final_pos[0],
            self.final_pos[1],
            self.final_pos[2],
            self.on_ground,
            self.teleports_0x40,
            self.corrections_0x40,
            self.max_prediction_drift,
            self.view_distance,
            self.lod_stored_columns,
            self.lod_fine_drawn,
            self.lod_coarse_drawn,
            self.health,
            self.food,
            self.keep_alives,
            self.packets_rx,
            self.packets_tx,
        )
    }
}

fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

struct SessionRunner {
    client: ProtocolClient,
    world: WorldState,
    player: PlayerPhysics,
    /// Far-terrain LOD store driven by the resolved `ClientSettings`
    /// (`None` when `distant_horizon` is off).
    lod: Option<LodManager>,
    entity_id: i32,
    play_ok: bool,
    initial_spawn_done: bool,
    teleports_0x40: u64,
    corrections_0x40: u64,
    max_prediction_drift: f64,
    chunk_batches: u64,
    keep_alives: u64,
    health: f32,
    food: i32,
    /// `system_chat` packets seen this session — retained so a live session
    /// can surface chat/decoded-component coverage in diagnostics.
    system_chats: u64,
    last_chunk_at: Option<Instant>,
}

impl SessionRunner {
    fn handle_event(&mut self, event: PlayEvent) -> Result<(), String> {
        match event {
            PlayEvent::JoinGame {
                entity_id,
                dimension,
            } => {
                self.entity_id = entity_id;
                self.play_ok = true;
                self.world.reset_dimension(dimension);
            }
            PlayEvent::Respawn { dimension } => {
                self.world.reset_dimension(dimension);
                self.initial_spawn_done = false;
            }
            PlayEvent::KeepAlive(id) => {
                self.client.send_keep_alive(id)?;
                self.keep_alives += 1;
            }
            PlayEvent::Teleport(tp) => {
                // Apply first so 26.x can echo the resulting pose back in
                // `accept_teleportation`; on 767 the wire output is identical.
                let drift = self.player.apply_teleport(&tp);
                self.client.send_accept_teleportation(
                    tp.teleport_id,
                    self.player.pos,
                    self.player.yaw,
                    self.player.pitch,
                    self.player.on_ground,
                )?;
                self.teleports_0x40 += 1;
                if self.initial_spawn_done {
                    self.corrections_0x40 += 1;
                    if drift > self.max_prediction_drift {
                        self.max_prediction_drift = drift;
                    }
                } else {
                    self.initial_spawn_done = true;
                }
            }
            PlayEvent::ChunkBatchStart => {}
            PlayEvent::ChunkData(chunk) => {
                let (cx, cz) = (chunk.cx, chunk.cz);
                self.world.apply_chunk(*chunk)?;
                self.last_chunk_at = Some(Instant::now());
                if let Some(lod) = self.lod.as_mut() {
                    if let Some(sections) = self.world.column_sections(cx, cz) {
                        let views: Vec<_> = sections.iter().map(|s| s.view()).collect();
                        lod.insert_column(cx, cz, &views);
                    }
                }
            }
            PlayEvent::ChunkBatchFinished(_size) => {
                self.client.send_chunk_batch_received(30.0)?;
                self.chunk_batches += 1;
            }
            PlayEvent::UpdateLight { cx, cz, light } => {
                self.world.apply_light_update(cx, cz, &light)?;
            }
            PlayEvent::BlockUpdate { pos, state_id } => {
                self.world.apply_block_update(pos, state_id)?;
            }
            PlayEvent::SectionBlocksUpdate(updates) => {
                for (pos, state_id) in updates {
                    self.world.apply_block_update(pos, state_id)?;
                }
            }
            PlayEvent::UnloadChunk { cx, cz } => {
                self.world.unload_chunk(cx, cz);
                if let Some(lod) = self.lod.as_mut() {
                    lod.remove_column(cx, cz);
                }
            }
            PlayEvent::SetCenterChunk { .. } => {}
            PlayEvent::SetHealth { health, food, .. } => {
                self.health = health;
                self.food = food;
            }
            PlayEvent::SystemChat { .. } => {
                self.system_chats += 1;
            }
            PlayEvent::Disconnect(reason) => {
                return Err(format!("server disconnected during play: {reason}"));
            }
            PlayEvent::Other(_) => {}
        }
        Ok(())
    }

    fn poll_once(&mut self, timeout: Duration) -> Result<bool, String> {
        if let Some(event) = self.client.recv_play(timeout)? {
            self.handle_event(event)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

pub fn run_session(cfg: &CliConfig) -> Result<SessionSummary, String> {
    let Some((host, port)) = cfg.target_endpoint()? else {
        return Err(
            "no server target configured (pass --quickPlayMultiplayer <host[:port]> or --server <host> [--port <port>])"
                .to_string(),
        );
    };
    let uuid = match &cfg.uuid {
        Some(raw) => parse_uuid(raw)?,
        None => offline_uuid(&cfg.username),
    };
    let blocks = BlockTable::load(cfg.assets_json.as_deref());
    let (mut client, handshake) = ProtocolClient::connect_for_version(
        &host,
        port,
        &cfg.username,
        uuid,
        cfg.view_distance,
        Duration::from_secs(15),
        cfg.mc_version,
    )?;

    let world = match cfg.version_dir.as_deref() {
        Some(dir) => WorldState::new_from_version_dir(client.dimension.clone(), blocks, dir)?,
        None => WorldState::new(client.dimension.clone(), blocks),
    };
    client.block_state_count = world.state_count;
    // Resolved player settings drive the renderer, the far-terrain LOD
    // radii, and (via `cfg.view_distance`) the requested chunk window.
    let render_config = crate::render::RenderConfig::from_settings(&cfg.client_settings);
    let mut runner = SessionRunner {
        client,
        world,
        player: PlayerPhysics::new([0.0, 64.0, 0.0], 0.0, 0.0),
        lod: cfg.client_settings.to_lod_config().map(LodManager::new),
        entity_id: 0,
        play_ok: false,
        initial_spawn_done: false,
        teleports_0x40: 0,
        corrections_0x40: 0,
        max_prediction_drift: 0.0,
        chunk_batches: 0,
        keep_alives: 0,
        health: 20.0,
        food: 20,
        system_chats: 0,
        last_chunk_at: None,
    };

    let expected_window = ((2 * i32::from(cfg.view_distance) + 1) as usize).pow(2);
    // Generous bound: an integrated server may still be generating the spawn
    // chunk window when the session starts. The loop exits as soon as the
    // window is ready, so a healthy server never waits this long.
    let sync_deadline = Instant::now() + Duration::from_secs(120);

    // Frustum planes for dirty-section rebuild prioritization (in-frustum and
    // near-camera sections mesh first). Aspect follows the configured frame.
    let render_aspect = f64::from(cfg.render_width) / f64::from(cfg.render_height.max(1));
    let player_planes = |runner: &SessionRunner| {
        let cam = runner.player.camera();
        (
            cam,
            crate::render::frustum_planes_fov(
                cam,
                runner.player.yaw,
                runner.player.pitch,
                render_aspect,
                render_config.fov_deg,
            ),
        )
    };

    // Phase 1: Receive JoinGame + initial spawn Teleport + initial chunk window.
    while Instant::now() < sync_deadline {
        let ready = runner.play_ok
            && runner.initial_spawn_done
            && (runner.world.loaded_chunks() >= expected_window
                || (runner.world.loaded_chunks() > 0
                    && runner
                        .last_chunk_at
                        .is_some_and(|t| t.elapsed() >= Duration::from_millis(500))));
        if ready {
            break;
        }
        runner.poll_once(Duration::from_millis(50))?;
        let (cam, planes) = player_planes(&runner);
        runner
            .world
            .rebuild_dirty_bounded_ordered(16, cam, &planes)?;
    }

    if !runner.play_ok || !runner.initial_spawn_done {
        return Err(
            "timed out waiting for JoinGame (0x2B) and initial Teleport (0x40)".to_string(),
        );
    }

    // Phase 2: 20 Hz physics prediction + bounded greedy meshing loop.
    let tick_period = Duration::from_millis(50);
    for tick in 0..cfg.ticks {
        let tick_start = Instant::now();

        while runner.poll_once(Duration::from_millis(1))? {}

        let input = script_input(cfg.script, tick, runner.player.on_ground);
        let outcome = runner.player.step(&runner.world, input)?;

        if cfg.script == ScriptMode::Survey {
            runner.client.send_player_position_rotation(
                outcome.pos[0],
                outcome.pos[1],
                outcome.pos[2],
                outcome.yaw,
                outcome.pitch,
                outcome.on_ground,
            )?;
        } else {
            runner.client.send_player_position(
                outcome.pos[0],
                outcome.pos[1],
                outcome.pos[2],
                outcome.on_ground,
            )?;
        }

        let (cam, planes) = player_planes(&runner);
        runner
            .world
            .rebuild_dirty_bounded_ordered(16, cam, &planes)?;

        while let Some(rem) = tick_period.checked_sub(tick_start.elapsed()) {
            if rem.is_zero() {
                break;
            }
            runner.poll_once(rem.min(Duration::from_millis(10)))?;
        }
    }

    // Phase 3: Finish rebuilding any remaining dirty sections and drain trailing replies.
    let drain_deadline = Instant::now() + Duration::from_millis(150);
    while let Some(rem) = drain_deadline.checked_duration_since(Instant::now()) {
        if rem.is_zero() {
            break;
        }
        if !runner.poll_once(rem.min(Duration::from_millis(25)))? {
            break;
        }
    }
    while runner.world.pending_mesh_sections() > 0 {
        let (cam, planes) = player_planes(&runner);
        runner
            .world
            .rebuild_dirty_bounded_ordered(64, cam, &planes)?;
    }

    // Optional software-rasterized PNG frame from the final camera pose.
    let mut render_ms = 0.0;
    let mut triangles_drawn = 0u64;
    let mut pixels_shaded = 0u64;
    let mut drawn_sections = 0u64;
    let mut culled_sections = 0u64;
    let mut region_draw_calls = 0u64;
    let mut buckets_drawn = 0u64;
    let mut buckets_culled = 0u64;
    if let Some(ref png_path) = cfg.render_png {
        let cam = runner.player.camera();
        let eye = [f64::from(cam[0]), f64::from(cam[1]), f64::from(cam[2])];
        let frame = crate::render::render_frame_with_config(
            &runner.world,
            eye,
            runner.player.yaw,
            runner.player.pitch,
            cfg.render_width,
            cfg.render_height,
            &render_config,
        )?;
        frame.write_png(png_path)?;
        render_ms = frame.stats.render_ms;
        triangles_drawn = frame.stats.triangles_drawn;
        pixels_shaded = frame.stats.pixels_shaded;
        drawn_sections = frame.stats.drawn_sections;
        culled_sections = frame.stats.culled_sections;
        region_draw_calls = frame.stats.region_draw_calls;
        buckets_drawn = frame.stats.buckets_drawn;
        buckets_culled = frame.stats.buckets_culled;
    }

    // Far-terrain LOD bookkeeping: which stored columns draw this frame,
    // and at which tier, measured from the final player chunk.
    let mut lod_stored_columns = 0usize;
    let mut lod_fine_drawn = 0usize;
    let mut lod_coarse_drawn = 0usize;
    if let Some(lod) = runner.lod.as_mut() {
        let camera_chunk = (
            (runner.player.pos[0] / 16.0).floor() as i32,
            (runner.player.pos[2] / 16.0).floor() as i32,
        );
        let loaded_full: HashSet<(i32, i32)> = runner.world.columns.keys().copied().collect();
        let _ = lod.active_lod_columns(camera_chunk, &loaded_full);
        let stats = lod.stats();
        lod_stored_columns = stats.stored_columns;
        lod_fine_drawn = stats.fine_drawn;
        lod_coarse_drawn = stats.coarse_drawn;
    }

    Ok(SessionSummary {
        username: handshake.server_username,
        server: format!("{host}:{port}"),
        protocol: runner.client.protocol_version.number(),
        login_ok: handshake.login_ok,
        config_ok: handshake.config_ok,
        play_ok: runner.play_ok,
        compression_threshold: handshake.compression_threshold,
        dimension: runner.world.dimension.name.clone(),
        entity_id: runner.entity_id,
        script: cfg.script,
        ticks_executed: cfg.ticks,
        chunks_decoded: runner.world.chunks_decoded,
        chunks_loaded: runner.world.loaded_chunks(),
        chunks_unloaded: runner.world.chunks_unloaded,
        chunk_batches: runner.chunk_batches,
        light_updates: runner.world.light_updates,
        block_updates: runner.world.block_updates,
        local_light_edits: runner.world.local_light_edits,
        sections_rebuilt: runner.world.metrics.sections_rebuilt,
        total_quads: runner.world.metrics.total_quads,
        packed_vertices: runner.world.metrics.packed_vertices,
        translucent_quads: runner.world.metrics.translucent_quads,
        avg_mesh_us_per_section: runner.world.metrics.avg_us_per_section(),
        total_mesh_us: runner.world.metrics.total_mesh_us,
        total_pack_us: runner.world.metrics.total_pack_us,
        total_sort_us: runner.world.metrics.total_sort_us,
        block_states_from_json: runner.world.blocks.loaded_from_json,
        teleports_0x40: runner.teleports_0x40,
        corrections_0x40: runner.corrections_0x40,
        max_prediction_drift: runner.max_prediction_drift,
        final_pos: runner.player.pos,
        on_ground: runner.player.on_ground,
        health: runner.health,
        food: runner.food,
        keep_alives: runner.keep_alives,
        packets_rx: runner.client.packets_rx,
        packets_tx: runner.client.packets_tx,
        view_distance: cfg.view_distance,
        lod_stored_columns,
        lod_fine_drawn,
        lod_coarse_drawn,
        render_ms,
        triangles_drawn,
        pixels_shaded,
        drawn_sections,
        culled_sections,
        region_draw_calls,
        buckets_drawn,
        buckets_culled,
    })
}
