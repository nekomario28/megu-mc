// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Client-side 20 Hz physics prediction (`client-kernel`), scripted movement loop,
//! and headless Protocol 767 session runner.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use megu_kernel::player_physics::{
    step_player_physics, PlayerInputState, PlayerPhysicsState, SurfaceKind,
};
use megu_kernel::raycast::{raycast_blocks, RayBlockShapes};
use megu_kernel::{Aabb, FluidClipMode, RayHit};

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
    pub sneak: bool,
    pub sprint: bool,
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
    kernel_state: PlayerPhysicsState,
}

fn surface_kind(world: &WorldState, pos: [i32; 3]) -> SurfaceKind {
    let Some(state_id) = world.get_block_state(pos[0], pos[1], pos[2]) else {
        return SurfaceKind::Normal;
    };
    let name = world
        .registry
        .as_ref()
        .and_then(|registry| registry.state_name(state_id).ok());
    match name {
        Some("minecraft:ice") => SurfaceKind::Ice,
        Some("minecraft:packed_ice") => SurfaceKind::PackedIce,
        Some("minecraft:blue_ice") => SurfaceKind::BlueIce,
        Some("minecraft:slime_block") => SurfaceKind::Slime,
        Some("minecraft:honey_block") => SurfaceKind::Honey,
        Some("minecraft:soul_sand") => SurfaceKind::SoulSand,
        Some("minecraft:cobweb") => SurfaceKind::Cobweb,
        Some("minecraft:ladder") => SurfaceKind::Ladder,
        Some("minecraft:vine") => SurfaceKind::Vine,
        Some("minecraft:scaffolding") => SurfaceKind::Scaffolding,
        Some("minecraft:powder_snow") => SurfaceKind::PowderSnow,
        _ => match state_id {
            // Vanilla 1.21.1 state ids used by BlockTable::builtin() when the
            // version-pack registry is unavailable.
            5780 => SurfaceKind::Ice,
            10746 => SurfaceKind::PackedIce,
            12941 => SurfaceKind::BlueIce,
            10364 => SurfaceKind::Slime,
            19445 => SurfaceKind::Honey,
            5850 => SurfaceKind::SoulSand,
            2004 => SurfaceKind::Cobweb,
            4654..=4661 => SurfaceKind::Ladder,
            6837..=6868 => SurfaceKind::Vine,
            18372..=18403 => SurfaceKind::Scaffolding,
            22318 => SurfaceKind::PowderSnow,
            _ => name.map(modded_surface_kind).unwrap_or(SurfaceKind::Normal),
        },
    }
}

/// Namespace-agnostic surface classification for modded block names: any
/// namespace's `*_ice`, `*packed_ice`, `*blue_ice`, `*slime_block`,
/// `*honey_block`, `*soul_sand`, `*ladder*`, `*scaffold*`, `*cobweb*`, or
/// `*_web` behaves physically like its vanilla counterpart. Exact vanilla
/// names are matched upstream in [`surface_kind`]; this only sees registry
/// names the exact list missed.
fn modded_surface_kind(name: &str) -> SurfaceKind {
    let path = name.rsplit(':').next().unwrap_or(name);
    if path.ends_with("blue_ice") {
        SurfaceKind::BlueIce
    } else if path.ends_with("packed_ice") {
        SurfaceKind::PackedIce
    } else if path.ends_with("_ice") {
        SurfaceKind::Ice
    } else if path.ends_with("slime_block") {
        SurfaceKind::Slime
    } else if path.ends_with("honey_block") {
        SurfaceKind::Honey
    } else if path.ends_with("soul_sand") {
        SurfaceKind::SoulSand
    } else if path.contains("ladder") {
        SurfaceKind::Ladder
    } else if path.contains("scaffold") {
        SurfaceKind::Scaffolding
    } else if path.contains("cobweb") || path.ends_with("_web") {
        SurfaceKind::Cobweb
    } else {
        SurfaceKind::Normal
    }
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
            kernel_state: PlayerPhysicsState {
                pos,
                ..PlayerPhysicsState::default()
            },
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
        self.in_water = false;
        self.in_lava = false;
        self.kernel_state = PlayerPhysicsState {
            pos: self.pos,
            on_ground: self.on_ground,
            ..PlayerPhysicsState::default()
        };
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

        // Include the entire velocity sweep, jump impulse, step-up clearance,
        // and the support probes used by the kernel's sneak edge protection.
        let bounds = self.aabb();
        let margin = self.vel.map(|velocity| velocity.abs() + 1.0);
        let sweep = Aabb {
            min: std::array::from_fn(|axis| bounds.min[axis] - margin[axis]),
            max: std::array::from_fn(|axis| bounds.max[axis] + margin[axis]),
        };
        let shapes = world.collect_collision_aabbs(sweep);

        let feet = self.pos.map(|v| v.floor() as i32);
        let inside = surface_kind(world, feet);
        let surface = if matches!(
            inside,
            SurfaceKind::Ladder
                | SurfaceKind::Vine
                | SurfaceKind::Scaffolding
                | SurfaceKind::Cobweb
                | SurfaceKind::PowderSnow
        ) {
            inside
        } else {
            surface_kind(
                world,
                [feet[0], (self.pos[1] - 0.500_000_1).floor() as i32, feet[2]],
            )
        };

        let state = PlayerPhysicsState {
            pos: self.pos,
            vel: self.vel,
            on_ground: self.on_ground,
            ultra_warm: world.dimension.name == "minecraft:the_nether",
            ..self.kernel_state
        };
        let result = step_player_physics(
            state,
            PlayerInputState {
                // TickInput historically expressed movement in blocks/tick.
                // Convert that scale to the kernel's normalized controls.
                forward: (input.forward * 5.0) as f32,
                strafe: (input.strafe * 5.0) as f32,
                jump: input.jump,
                sneak: input.sneak,
                sprint: input.sprint,
                yaw_deg: self.yaw,
                pitch_deg: self.pitch,
                ..PlayerInputState::default()
            },
            surface,
            &shapes,
            |x, y, z| Ok(world.fluid_cell_at(x, y, z)),
        )
        .map_err(|e| format!("step_player_physics failed: {e:?}"))?;

        self.kernel_state = result.state;
        self.pos = result.state.pos;
        self.vel = result.state.vel;
        self.on_ground = result.state.on_ground;
        self.in_water = result.state.in_water;
        self.in_lava = result.state.in_lava;

        Ok(StepOutcome {
            pos: self.pos,
            yaw: self.yaw,
            pitch: self.pitch,
            on_ground: self.on_ground,
            in_fluid: self.in_water || self.in_lava,
            horizontal_collision: result.collided_horizontal,
            stepped_up: result.stepped_up,
        })
    }

    pub fn pick_target_block(
        &self,
        world: &WorldState,
        max_distance: f64,
        fluid_mode: FluidClipMode,
    ) -> Option<RayHit> {
        let origin = [self.pos[0], self.pos[1] + PLAYER_EYE_HEIGHT, self.pos[2]];
        let yaw = f64::from(self.yaw).to_radians();
        let pitch = f64::from(self.pitch).to_radians();
        let direction = [
            -yaw.sin() * pitch.cos(),
            -pitch.sin(),
            yaw.cos() * pitch.cos(),
        ];

        raycast_blocks(origin, direction, max_distance, fluid_mode, |x, y, z| {
            let Some(state) = world.get_block_state(x, y, z) else {
                return RayBlockShapes::default();
            };
            let mut boxes = Vec::new();
            world.blocks.push_collision_aabbs(state, x, y, z, &mut boxes);
            for shape in &mut boxes {
                for axis in 0..3 {
                    let offset = f64::from([x, y, z][axis]);
                    shape.min[axis] -= offset;
                    shape.max[axis] -= offset;
                }
            }
            let fluid = world.fluid_cell_at(x, y, z);
            RayBlockShapes {
                boxes,
                is_fluid: fluid.kind.is_some(),
                is_fluid_source: fluid.kind.is_some() && fluid.amount == 8 && !fluid.falling,
            }
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
            sprint: true,
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
                "megu-client session complete (Protocol {})\n",
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
    /// Shared acoustic cache the audio runtime's mixer traces against —
    /// `sync_chunk`/`sync_block` keep it live as the world changes.
    acoustics: crate::session::WorldAcousticSnapshot,
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
    /// Death & respawn loop state — gates the tick loop's movement
    /// packets and drives the headless `PERFORM_RESPAWN` auto-request.
    death: crate::session::DeathLoopState,
    /// Ticks until an unacknowledged respawn request is retried.
    death_retry: u32,
    /// `system_chat` packets seen this session — retained so a live session
    /// can surface chat/decoded-component coverage in diagnostics.
    system_chats: u64,
    last_chunk_at: Option<Instant>,
}

impl SessionRunner {
    fn handle_event(&mut self, event: PlayEvent) -> Result<(), String> {
        // The death tracker sees every play event first: `JoinGame`
        // seeds `hardcore`, `player_combat_kill`/`set_health(<=0)` mark
        // death, and `respawn`/`set_health(>0)` clear it.
        self.death.apply(&event);
        match event {
            PlayEvent::JoinGame {
                entity_id,
                dimension,
                ..
            } => {
                self.entity_id = entity_id;
                self.play_ok = true;
                self.world.reset_dimension(dimension);
                self.acoustics.reset_dimension(&self.world);
            }
            PlayEvent::Respawn { dimension } => {
                self.world.reset_dimension(dimension);
                self.acoustics.reset_dimension(&self.world);
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
                self.acoustics.sync_chunk(&self.world, cx, cz);
                self.refresh_audio_acoustics();
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
                self.acoustics.sync_block(&self.world, pos);
                self.refresh_audio_acoustics();
            }
            PlayEvent::SectionBlocksUpdate(updates) => {
                for (pos, state_id) in updates {
                    self.world.apply_block_update(pos, state_id)?;
                    self.acoustics.sync_block(&self.world, pos);
                }
                self.refresh_audio_acoustics();
            }
            PlayEvent::UnloadChunk { cx, cz } => {
                self.world.unload_chunk(cx, cz);
                if let Some(lod) = self.lod.as_mut() {
                    lod.remove_column(cx, cz);
                }
                self.acoustics.unload_chunk(cx, cz);
                self.refresh_audio_acoustics();
            }
            PlayEvent::RemoveEntities { ref eids } => {
                self.world.apply_play_event(&event)?;
                if let Some(rt) = &mut self.client.audio {
                    for &eid in eids {
                        rt.remove_entity_position(eid);
                    }
                }
            }
            PlayEvent::AddEntity { eid, .. }
            | PlayEvent::MoveEntityPos { eid, .. }
            | PlayEvent::MoveEntityPosRot { eid, .. }
            | PlayEvent::MoveEntityRot { eid, .. }
            | PlayEvent::TeleportEntity { eid, .. }
            | PlayEvent::SetEntityMotion { eid, .. } => {
                self.world.apply_play_event(&event)?;
                if let Some(rt) = &mut self.client.audio {
                    if let Some(ent) = self.world.entities.get(&eid) {
                        rt.update_entity_position(eid, ent.pos);
                    }
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
            // Extended 767 play families (entities, world, containers, HUD)
            // decode into typed events; the session runner does not consume
            // them yet, so they are acknowledged and dropped here.
            _ => {}
        }
        Ok(())
    }

    /// Tell the audio mixer's spatial voices the acoustic scene changed so
    /// occlusion/reverb retraces against fresh chunk data.
    fn refresh_audio_acoustics(&mut self) {
        if let Some(rt) = &mut self.client.audio {
            rt.mixer.refresh_acoustics();
        }
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
    run_session_inner(cfg, None)
}

/// Run a session that checks `shutdown` at each bounded polling/tick boundary.
pub fn run_session_with_shutdown(
    cfg: &CliConfig,
    shutdown: &crate::shutdown::ShutdownFlag,
) -> Result<SessionSummary, String> {
    run_session_inner(cfg, Some(shutdown))
}

fn run_session_inner(
    cfg: &CliConfig,
    shutdown: Option<&crate::shutdown::ShutdownFlag>,
) -> Result<SessionSummary, String> {
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
    let (mut client, handshake) = ProtocolClient::connect_for_version_with_auth(
        &host,
        port,
        &cfg.username,
        uuid,
        cfg.view_distance,
        Duration::from_secs(15),
        cfg.mc_version,
        crate::session::online_auth_options(cfg),
    )?;

    let world = match cfg.version_dir.as_deref() {
        Some(dir) => WorldState::new_from_version_dir(client.dimension.clone(), blocks, dir)?,
        None => WorldState::new(client.dimension.clone(), blocks),
    };
    client.block_state_count = world.state_count;
    // Resolved player settings drive the renderer, the far-terrain LOD
    // radii, and (via `cfg.view_distance`) the requested chunk window.
    let render_config = crate::render::RenderConfig::from_settings(&cfg.client_settings);
    // The mixer traces occlusion/reverb against a shared acoustic snapshot
    // of the live world; `sync_chunk`/`sync_block` calls in `handle_event`
    // keep it current as blocks change.
    let acoustics = crate::session::WorldAcousticSnapshot::new(&world);
    if let Some(rt) = &mut client.audio {
        rt.mixer.set_world(Box::new(acoustics.clone()));
    }
    let mut runner = SessionRunner {
        client,
        world,
        acoustics,
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
        death: crate::session::DeathLoopState::default(),
        death_retry: 0,
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
        if shutdown.is_some_and(crate::shutdown::ShutdownFlag::is_requested) {
            return Err("session shutdown requested".into());
        }
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
        if shutdown.is_some_and(crate::shutdown::ShutdownFlag::is_requested) {
            return Err("session shutdown requested".into());
        }
        let tick_start = Instant::now();

        while runner.poll_once(Duration::from_millis(1))? {}

        let input = script_input(cfg.script, tick, runner.player.on_ground);
        let outcome = runner.player.step(&runner.world, input)?;

        if runner.death.is_dead() {
            // Vanilla sends no movement while dead. The headless runner
            // stands in for the DeathScreen's Respawn button: issue
            // `client_command(PERFORM_RESPAWN)` once — hardcore deaths
            // never request (vanilla offers no respawn there) — and
            // retry slowly while the server leaves it unacknowledged.
            if runner.death.can_request_respawn()
                || (runner.death.respawn_pending() && runner.death_retry == 0)
            {
                runner.client.send_perform_respawn()?;
                runner.death.note_respawn_sent();
                runner.death_retry = 40;
            }
            runner.death_retry = runner.death_retry.saturating_sub(1);
        } else if cfg.script == ScriptMode::Survey {
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
            if shutdown.is_some_and(crate::shutdown::ShutdownFlag::is_requested) {
                return Err("session shutdown requested".into());
            }
            if rem.is_zero() {
                break;
            }
            runner.poll_once(rem.min(Duration::from_millis(10)))?;
        }
    }

    // Phase 3: Finish rebuilding any remaining dirty sections and drain trailing replies.
    let drain_deadline = Instant::now() + Duration::from_millis(150);
    while let Some(rem) = drain_deadline.checked_duration_since(Instant::now()) {
        if shutdown.is_some_and(crate::shutdown::ShutdownFlag::is_requested) {
            break;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modded_surface_names_classify_like_their_vanilla_counterparts() {
        let cases = [
            ("mymod:frost_ice", SurfaceKind::Ice),
            ("mymod:packed_ice", SurfaceKind::PackedIce),
            ("mymod:blue_ice", SurfaceKind::BlueIce),
            ("quark:frosted_packed_ice", SurfaceKind::PackedIce),
            ("mymod:slime_block", SurfaceKind::Slime),
            ("mymod:sticky_slime_block", SurfaceKind::Slime),
            ("mymod:honey_block", SurfaceKind::Honey),
            ("mymod:soul_sand", SurfaceKind::SoulSand),
            ("mymod:scorched_soul_sand", SurfaceKind::SoulSand),
            ("create:andesite_ladder", SurfaceKind::Ladder),
            ("mymod:rope_ladder_top", SurfaceKind::Ladder),
            ("mymod:bamboo_scaffolding", SurfaceKind::Scaffolding),
            ("mymod:scaffold_bridge", SurfaceKind::Scaffolding),
            ("mymod:spider_web", SurfaceKind::Cobweb),
            ("mymod:cobweb_corner", SurfaceKind::Cobweb),
        ];
        for (name, expected) in cases {
            assert_eq!(modded_surface_kind(name), expected, "{name}");
        }
        // Names with no surface suffix stay Normal — including bare `ice`,
        // which the vanilla exact-match arm owns upstream.
        for name in [
            "minecraft:ice",
            "mymod:ice",
            "mymod:stone",
            "mymod:webbed",
            "mymod:lattice",
        ] {
            assert_eq!(modded_surface_kind(name), SurfaceKind::Normal, "{name}");
        }
    }
}
