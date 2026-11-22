// SPDX-License-Identifier: LGPL-3.0-or-later
//! Deterministic Minecraft 1.21.1 debug-key actions and world-space line geometry.
//!
//! The window layer supplies its current player/target snapshot, handles the
//! returned side effect, and calls [`finish_gamemode_switcher`] on F3 release.

use crate::Aabb;

/// The fifteen F3 combinations exposed by the client debug-chord kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum F3Chord {
    A,
    B,
    C,
    D,
    F4,
    G,
    H,
    I,
    N,
    P,
    Q,
    T,
    Key1,
    Key2,
    Key3,
}

pub const ALL_F3_CHORDS: [F3Chord; 15] = [
    F3Chord::A,
    F3Chord::B,
    F3Chord::C,
    F3Chord::D,
    F3Chord::F4,
    F3Chord::G,
    F3Chord::H,
    F3Chord::I,
    F3Chord::N,
    F3Chord::P,
    F3Chord::Q,
    F3Chord::T,
    F3Chord::Key1,
    F3Chord::Key2,
    F3Chord::Key3,
];

impl F3Chord {
    /// GLFW key codes used by the 1.21.1 keyboard handler.
    pub const fn glfw_key(self) -> i32 {
        match self {
            Self::A => 65,
            Self::B => 66,
            Self::C => 67,
            Self::D => 68,
            Self::F4 => 293,
            Self::G => 71,
            Self::H => 72,
            Self::I => 73,
            Self::N => 78,
            Self::P => 80,
            Self::Q => 81,
            Self::T => 84,
            Self::Key1 => 49,
            Self::Key2 => 50,
            Self::Key3 => 51,
        }
    }

    pub const fn from_glfw_key(key: i32) -> Option<Self> {
        match key {
            65 => Some(Self::A),
            66 => Some(Self::B),
            67 => Some(Self::C),
            68 => Some(Self::D),
            293 => Some(Self::F4),
            71 => Some(Self::G),
            72 => Some(Self::H),
            73 => Some(Self::I),
            78 => Some(Self::N),
            80 => Some(Self::P),
            81 => Some(Self::Q),
            84 => Some(Self::T),
            49 => Some(Self::Key1),
            50 => Some(Self::Key2),
            51 => Some(Self::Key3),
            _ => None,
        }
    }

    /// English fallback, including the shortcut, for each localization slot.
    pub const fn help_line(self) -> &'static str {
        match self {
            Self::A => "F3 + A = Reload chunks",
            Self::B => "F3 + B = Toggle hitboxes",
            Self::C => "F3 + C = Copy location",
            Self::D => "F3 + D = Clear chat",
            Self::F4 => "F3 + F4 = Open or cycle game mode switcher",
            Self::G => "F3 + G = Toggle chunk borders",
            Self::H => "F3 + H = Toggle advanced tooltips",
            Self::I => "F3 + I = Copy targeted block or entity data",
            Self::N => "F3 + N = Toggle spectator and previous game mode",
            Self::P => "F3 + P = Toggle pause on lost focus",
            Self::Q => "F3 + Q = Show debug shortcut help",
            Self::T => "F3 + T = Reload resource packs",
            Self::Key1 => "F3 + 1 = Toggle profiler pie chart",
            Self::Key2 => "F3 + 2 = Toggle FPS/TPS charts",
            Self::Key3 => "F3 + 3 = Toggle network/ping charts",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GameMode {
    Creative,
    Survival,
    Adventure,
    Spectator,
}

impl GameMode {
    pub const fn command_name(self) -> &'static str {
        match self {
            Self::Creative => "creative",
            Self::Survival => "survival",
            Self::Adventure => "adventure",
            Self::Spectator => "spectator",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::Creative => Self::Survival,
            Self::Survival => Self::Adventure,
            Self::Adventure => Self::Spectator,
            Self::Spectator => Self::Creative,
        }
    }
}

/// Local client settings and view state. The window owns persistence and UI rendering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebugChordState {
    pub show_debug_overlay: bool,
    pub show_chunk_borders: bool,
    pub show_hitboxes: bool,
    pub advanced_tooltips: bool,
    pub pause_on_lost_focus: bool,
    pub pie_chart: bool,
    pub fps_tps_chart: bool,
    pub net_chart: bool,
    pub previous_gamemode: Option<GameMode>,
    pub gamemode_switcher: Option<GameMode>,
}

impl Default for DebugChordState {
    fn default() -> Self {
        Self {
            show_debug_overlay: false,
            show_chunk_borders: false,
            show_hitboxes: false,
            advanced_tooltips: false,
            pause_on_lost_focus: true,
            pie_chart: false,
            fps_tps_chart: false,
            net_chart: false,
            previous_gamemode: None,
            gamemode_switcher: None,
        }
    }
}

impl DebugChordState {
    /// Plain F3 is handled by the window, separately from the F3 combinations.
    pub fn toggle_debug_overlay(&mut self) {
        self.show_debug_overlay = !self.show_debug_overlay;
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum DebugChordAction {
    ChatFeedback(String),
    ClipboardCommand(String),
    ReloadChunks,
    ReloadResourcePacks,
    ClearChat,
    SendGameModeCommand(GameMode),
    HelpLines(Vec<String>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlayerDebugInfo<'a> {
    pub dimension: &'a str,
    pub position: [f64; 3],
    pub yaw_deg: f64,
    pub pitch_deg: f64,
    pub gamemode: GameMode,
}

/// Pre-serialized block state and NBT are supplied by the renderer/world layer.
/// `block_state` includes any `[property=value]` suffix. NBT includes braces.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DebugTarget<'a> {
    Block {
        position: [i32; 3],
        block_state: &'a str,
        nbt: Option<&'a str>,
    },
    Entity {
        entity_id: &'a str,
        position: [f64; 3],
        nbt: Option<&'a str>,
    },
}

/// All inputs required to turn a key combination into a deterministic action.
/// Callers may provide a localized help formatter for each of the 15 lines.
pub type HelpFormatter<'a> = dyn Fn(F3Chord, &str) -> String + 'a;

pub struct DebugChordContext<'a> {
    pub state: &'a mut DebugChordState,
    pub player: Option<PlayerDebugInfo<'a>>,
    pub target: Option<DebugTarget<'a>>,
    pub has_permission_level_2: bool,
    pub reduced_debug_info: bool,
    pub localized_help: Option<&'a HelpFormatter<'a>>,
}

impl<'a> DebugChordContext<'a> {
    pub fn new(state: &'a mut DebugChordState) -> Self {
        Self {
            state,
            player: None,
            target: None,
            has_permission_level_2: false,
            reduced_debug_info: false,
            localized_help: None,
        }
    }
}

fn toggle_feedback(name: &str, enabled: bool) -> DebugChordAction {
    DebugChordAction::ChatFeedback(format!(
        "{name}: {}",
        if enabled { "shown" } else { "hidden" }
    ))
}

/// Run a single debug chord. Returned commands are interpreted by the client,
/// which is responsible for clipboard, networking, chat, and resource reloads.
pub fn handle_f3_chord(chord: F3Chord, ctx: &mut DebugChordContext<'_>) -> DebugChordAction {
    match chord {
        F3Chord::A => DebugChordAction::ReloadChunks,
        F3Chord::B => {
            ctx.state.show_hitboxes = !ctx.state.show_hitboxes;
            toggle_feedback("Hitboxes", ctx.state.show_hitboxes)
        }
        F3Chord::C => match ctx.player {
            Some(player)
                if !ctx.reduced_debug_info
                    && player.position.iter().all(|v| v.is_finite())
                    && player.yaw_deg.is_finite()
                    && player.pitch_deg.is_finite() =>
            {
                DebugChordAction::ClipboardCommand(format!(
                    "/execute in {} run tp @s {:.2} {:.2} {:.2} {:.2} {:.2}",
                    player.dimension,
                    player.position[0],
                    player.position[1],
                    player.position[2],
                    player.yaw_deg,
                    player.pitch_deg
                ))
            }
            _ => DebugChordAction::ChatFeedback("Location unavailable".into()),
        },
        F3Chord::D => DebugChordAction::ClearChat,
        F3Chord::F4 => {
            if !ctx.has_permission_level_2 {
                return DebugChordAction::ChatFeedback(
                    "Game mode switcher requires operator permission".into(),
                );
            }
            let selected = match ctx.state.gamemode_switcher {
                Some(mode) => mode.next(),
                None => ctx.state.previous_gamemode.unwrap_or_else(|| {
                    match ctx.player.map(|player| player.gamemode) {
                        Some(GameMode::Creative) => GameMode::Survival,
                        _ => GameMode::Creative,
                    }
                }),
            };
            ctx.state.gamemode_switcher = Some(selected);
            DebugChordAction::ChatFeedback(format!("Game mode: {}", selected.command_name()))
        }
        F3Chord::G => {
            ctx.state.show_chunk_borders = !ctx.state.show_chunk_borders;
            toggle_feedback("Chunk borders", ctx.state.show_chunk_borders)
        }
        F3Chord::H => {
            ctx.state.advanced_tooltips = !ctx.state.advanced_tooltips;
            toggle_feedback("Advanced tooltips", ctx.state.advanced_tooltips)
        }
        F3Chord::I => {
            if ctx.reduced_debug_info {
                return DebugChordAction::ChatFeedback("Target data unavailable".into());
            }
            match ctx.target {
                Some(DebugTarget::Block {
                    position,
                    block_state,
                    nbt,
                }) => DebugChordAction::ClipboardCommand(format!(
                    "/setblock {} {} {} {}{}",
                    position[0],
                    position[1],
                    position[2],
                    block_state,
                    nbt.unwrap_or("")
                )),
                Some(DebugTarget::Entity {
                    entity_id,
                    position,
                    nbt,
                }) if position.iter().all(|v| v.is_finite()) => {
                    let suffix = nbt.map(|nbt| format!(" {nbt}")).unwrap_or_default();
                    DebugChordAction::ClipboardCommand(format!(
                        "/summon {} {:.2} {:.2} {:.2}{}",
                        entity_id, position[0], position[1], position[2], suffix
                    ))
                }
                _ => DebugChordAction::ChatFeedback("Target data unavailable".into()),
            }
        }
        F3Chord::N => {
            if !ctx.has_permission_level_2 {
                return DebugChordAction::ChatFeedback(
                    "Spectator toggle requires operator permission".into(),
                );
            }
            match ctx.player.map(|player| player.gamemode) {
                Some(GameMode::Spectator) => DebugChordAction::SendGameModeCommand(
                    ctx.state.previous_gamemode.unwrap_or(GameMode::Creative),
                ),
                Some(mode) => {
                    ctx.state.previous_gamemode = Some(mode);
                    DebugChordAction::SendGameModeCommand(GameMode::Spectator)
                }
                None => DebugChordAction::ChatFeedback("Game mode unavailable".into()),
            }
        }
        F3Chord::P => {
            ctx.state.pause_on_lost_focus = !ctx.state.pause_on_lost_focus;
            toggle_feedback("Pause on lost focus", ctx.state.pause_on_lost_focus)
        }
        F3Chord::Q => DebugChordAction::HelpLines(
            ALL_F3_CHORDS
                .iter()
                .copied()
                .map(|item| match ctx.localized_help {
                    Some(localize) => localize(item, item.help_line()),
                    None => item.help_line().to_string(),
                })
                .collect(),
        ),
        F3Chord::T => DebugChordAction::ReloadResourcePacks,
        F3Chord::Key1 => {
            ctx.state.pie_chart = !ctx.state.show_debug_overlay || !ctx.state.pie_chart;
            if ctx.state.pie_chart {
                ctx.state.show_debug_overlay = true;
            }
            toggle_feedback("Profiler chart", ctx.state.pie_chart)
        }
        F3Chord::Key2 => {
            ctx.state.fps_tps_chart = !ctx.state.show_debug_overlay || !ctx.state.fps_tps_chart;
            if ctx.state.fps_tps_chart {
                ctx.state.show_debug_overlay = true;
                ctx.state.net_chart = false;
            }
            toggle_feedback("FPS/TPS charts", ctx.state.fps_tps_chart)
        }
        F3Chord::Key3 => {
            ctx.state.net_chart = !ctx.state.show_debug_overlay || !ctx.state.net_chart;
            if ctx.state.net_chart {
                ctx.state.show_debug_overlay = true;
                ctx.state.fps_tps_chart = false;
            }
            toggle_feedback("Network/ping charts", ctx.state.net_chart)
        }
    }
}

/// Apply the highlighted F3+F4 mode when the F3 key is released.
/// Vanilla sends no command if the selected mode already matches the current mode.
pub fn finish_gamemode_switcher(
    state: &mut DebugChordState,
    current_mode: GameMode,
) -> Option<DebugChordAction> {
    let selected = state.gamemode_switcher.take()?;
    (selected != current_mode).then_some(DebugChordAction::SendGameModeCommand(selected))
}

/// One world-space segment, with unpremultiplied sRGB colors.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DebugLine {
    pub start: [f32; 3],
    pub end: [f32; 3],
    pub rgba: [u8; 4],
}

const RED_BORDER: [u8; 4] = [255, 0, 0, 128];
const YELLOW: [u8; 4] = [255, 255, 0, 255];
const DARK_CYAN: [u8; 4] = [0, 155, 155, 255];
const BLUE_BORDER: [u8; 4] = [64, 64, 255, 255];

fn line(lines: &mut Vec<DebugLine>, start: [f32; 3], end: [f32; 3], rgba: [u8; 4]) {
    lines.push(DebugLine { start, end, rgba });
}

fn ring(lines: &mut Vec<DebugLine>, x: f32, z: f32, y: f32, rgba: [u8; 4]) {
    let x2 = x + 16.0;
    let z2 = z + 16.0;
    line(lines, [x, y, z], [x2, y, z], rgba);
    line(lines, [x2, y, z], [x2, y, z2], rgba);
    line(lines, [x2, y, z2], [x, y, z2], rgba);
    line(lines, [x, y, z2], [x, y, z], rgba);
}

/// Build the 1.21.1 chunk-border grid in world coordinates. Input chunk
/// coordinates are chunk indices, and Y endpoints are inclusive.
///
/// Vanilla shows red corner pillars across the surrounding 3x3 chunks,
/// alternating yellow/dark-cyan 2-block subdivisions, and blue 16-block
/// section outlines of the focused chunk.
pub fn build_chunk_border_lines(
    chunk_x: i32,
    chunk_z: i32,
    min_y: i32,
    max_y: i32,
) -> Vec<DebugLine> {
    if min_y > max_y {
        return Vec::new();
    }

    let x = (i64::from(chunk_x) * 16) as f32;
    let z = (i64::from(chunk_z) * 16) as f32;
    let bottom = min_y as f32;
    let top = max_y as f32;
    let mut lines = Vec::new();

    for dx in [-16.0, 0.0, 16.0, 32.0] {
        for dz in [-16.0, 0.0, 16.0, 32.0] {
            line(
                &mut lines,
                [x + dx, bottom, z + dz],
                [x + dx, top, z + dz],
                RED_BORDER,
            );
        }
    }

    for offset in (2..16).step_by(2) {
        let delta = offset as f32;
        let color = if offset % 4 == 0 { DARK_CYAN } else { YELLOW };
        for face in [z, z + 16.0] {
            line(
                &mut lines,
                [x + delta, bottom, face],
                [x + delta, top, face],
                color,
            );
        }
        for face in [x, x + 16.0] {
            line(
                &mut lines,
                [face, bottom, z + delta],
                [face, top, z + delta],
                color,
            );
        }
    }

    for y in (min_y..=max_y).step_by(2) {
        let color = if y % 8 == 0 { DARK_CYAN } else { YELLOW };
        ring(&mut lines, x, z, y as f32, color);
    }

    for dx in [0.0, 16.0] {
        for dz in [0.0, 16.0] {
            line(
                &mut lines,
                [x + dx, bottom, z + dz],
                [x + dx, top, z + dz],
                BLUE_BORDER,
            );
        }
    }
    for y in (min_y..=max_y).step_by(16) {
        ring(&mut lines, x, z, y as f32, BLUE_BORDER);
    }
    lines
}

const HITBOX_WHITE: [u8; 4] = [255, 255, 255, 255];
const HITBOX_RED: [u8; 4] = [255, 0, 0, 255];
const HITBOX_BLUE: [u8; 4] = [0, 0, 255, 255];

/// Build a finite 12-edge AABB, the living eye-height rectangle and a 2-block
/// blue look ray. Nonfinite yaw/pitch become zero-degree rotation.
pub fn build_entity_hitbox_lines(
    aabb: Aabb,
    eye_y: f64,
    yaw_deg: f32,
    pitch_deg: f32,
    is_living: bool,
) -> Vec<DebugLine> {
    if !(0..3).all(|axis| {
        aabb.min[axis].is_finite()
            && aabb.max[axis].is_finite()
            && aabb.min[axis] < aabb.max[axis]
            && (aabb.min[axis] as f32).is_finite()
            && (aabb.max[axis] as f32).is_finite()
    }) {
        return Vec::new();
    }

    let min = aabb.min.map(|v| v as f32);
    let max = aabb.max.map(|v| v as f32);
    let mut lines = Vec::with_capacity(if is_living { 17 } else { 13 });
    for y in [min[1], max[1]] {
        for x in [min[0], max[0]] {
            line(&mut lines, [x, y, min[2]], [x, y, max[2]], HITBOX_WHITE);
        }
        for z in [min[2], max[2]] {
            line(&mut lines, [min[0], y, z], [max[0], y, z], HITBOX_WHITE);
        }
    }
    for x in [min[0], max[0]] {
        for z in [min[2], max[2]] {
            line(&mut lines, [x, min[1], z], [x, max[1], z], HITBOX_WHITE);
        }
    }

    let center_y = if eye_y.is_finite() && (eye_y as f32).is_finite() {
        eye_y as f32
    } else {
        (aabb.min[1] * 0.5 + aabb.max[1] * 0.5) as f32
    };
    if is_living {
        line(
            &mut lines,
            [min[0], center_y, min[2]],
            [max[0], center_y, min[2]],
            HITBOX_RED,
        );
        line(
            &mut lines,
            [max[0], center_y, min[2]],
            [max[0], center_y, max[2]],
            HITBOX_RED,
        );
        line(
            &mut lines,
            [max[0], center_y, max[2]],
            [min[0], center_y, max[2]],
            HITBOX_RED,
        );
        line(
            &mut lines,
            [min[0], center_y, max[2]],
            [min[0], center_y, min[2]],
            HITBOX_RED,
        );
    }

    let yaw = if yaw_deg.is_finite() { yaw_deg } else { 0.0 };
    let pitch = if pitch_deg.is_finite() {
        pitch_deg
    } else {
        0.0
    };
    let (sin_yaw, cos_yaw) = yaw.to_radians().sin_cos();
    let (sin_pitch, cos_pitch) = pitch.to_radians().sin_cos();
    let origin = [
        (min[0] * 0.5 + max[0] * 0.5),
        center_y,
        (min[2] * 0.5 + max[2] * 0.5),
    ];
    let tip = [
        origin[0] - 2.0 * sin_yaw * cos_pitch,
        origin[1] - 2.0 * sin_pitch,
        origin[2] + 2.0 * cos_yaw * cos_pitch,
    ];
    if origin.iter().chain(tip.iter()).all(|v| v.is_finite()) {
        line(&mut lines, origin, tip, HITBOX_BLUE);
    }
    lines
}

/// Return the advanced suffix of a Vanilla item tooltip. `damage` is consumed
/// durability. The two item metadata lines carry Vanilla dark-gray (§8).
pub fn format_advanced_tooltip_lines(
    item_id: &str,
    damage: u32,
    max_damage: u32,
    component_count: usize,
) -> Vec<String> {
    let mut lines = Vec::with_capacity(3);
    if max_damage > 0 && damage > 0 {
        lines.push(format!(
            "Durability: {} / {}",
            max_damage.saturating_sub(damage),
            max_damage
        ));
    }
    lines.push(format!("§8{item_id}"));
    if component_count > 0 {
        lines.push(format!("§8{component_count} component(s)"));
    }
    lines
}
