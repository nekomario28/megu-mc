// R7 GP-3: Redstone signal evaluation and scheduled updates.
// Reference: Mojang 1.21.1 server jar (SHA-1 59353fb40c36d304f2035d51e7d6e6baa98dc05c),
// SignalGetter, RedStoneWireBlock, RedstoneTorchBlock, DiodeBlock and their subclasses.

use std::cmp::Reverse;
use std::collections::{BTreeSet as RsBTreeSet, VecDeque};

include!("redstone_world.rs");

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)] // Vanilla face indices.
enum RsDir {
    Down,
    Up,
    North,
    South,
    West,
    East,
}

#[cfg_attr(not(test), allow(dead_code))]
impl RsDir {
    const ALL: [Self; 6] = [
        Self::Down,
        Self::Up,
        Self::North,
        Self::South,
        Self::West,
        Self::East,
    ];
    const HORIZONTAL: [Self; 4] = [Self::North, Self::South, Self::West, Self::East];

    fn offset(self) -> (i32, i32, i32) {
        match self {
            Self::Down => (0, -1, 0),
            Self::Up => (0, 1, 0),
            Self::North => (0, 0, -1),
            Self::South => (0, 0, 1),
            Self::West => (-1, 0, 0),
            Self::East => (1, 0, 0),
        }
    }

    fn opposite(self) -> Self {
        match self {
            Self::Down => Self::Up,
            Self::Up => Self::Down,
            Self::North => Self::South,
            Self::South => Self::North,
            Self::West => Self::East,
            Self::East => Self::West,
        }
    }

    fn left(self) -> Self {
        match self {
            Self::North => Self::West,
            Self::South => Self::East,
            Self::West => Self::South,
            Self::East => Self::North,
            Self::Down | Self::Up => self,
        }
    }

    fn right(self) -> Self {
        match self {
            Self::Down | Self::Up => self,
            _ => self.left().opposite(),
        }
    }

    fn horizontal_index(self) -> Option<usize> {
        Self::HORIZONTAL.iter().position(|&dir| dir == self)
    }

    fn from_property(value: &str) -> Option<Self> {
        Some(match value {
            "down" => Self::Down,
            "up" => Self::Up,
            "north" => Self::North,
            "south" => Self::South,
            "west" => Self::West,
            "east" => Self::East,
            _ => return None,
        })
    }

    fn property(self) -> &'static str {
        match self {
            Self::Down => "down",
            Self::Up => "up",
            Self::North => "north",
            Self::South => "south",
            Self::West => "west",
            Self::East => "east",
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RsPos(i32, i32, i32);

#[cfg_attr(not(test), allow(dead_code))]
impl RsPos {
    fn relative(self, dir: RsDir) -> Self {
        let (dx, dy, dz) = dir.offset();
        Self(self.0 + dx, self.1 + dy, self.2 + dz)
    }

    fn above(self) -> Self {
        self.relative(RsDir::Up)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RsWireConnection {
    None,
    Side,
    Up,
}

impl RsWireConnection {
    fn from_property(value: &str) -> Option<Self> {
        Some(match value {
            "none" => Self::None,
            "side" => Self::Side,
            "up" => Self::Up,
            _ => return None,
        })
    }

    fn property(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Side => "side",
            Self::Up => "up",
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RedStoneWireBlock {
    power: u8,
    // Indexed by RsDir::HORIZONTAL. An isolated wire can retain its dot or cross shape.
    connections: [RsWireConnection; 4],
}

impl Default for RedStoneWireBlock {
    fn default() -> Self {
        Self {
            power: 0,
            connections: [RsWireConnection::Side; 4],
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl RedStoneWireBlock {
    fn signal(self, toward: RsDir) -> u8 {
        if toward == RsDir::Down
            || toward
                .horizontal_index()
                .is_some_and(|index| self.connections[index] != RsWireConnection::None)
        {
            self.power
        } else {
            0
        }
    }

    fn with_connections(self, world: &impl RedstoneWorld, pos: RsPos) -> Self {
        let mut connections = RsDir::HORIZONTAL.map(|dir| rs_wire_connection(world, pos, dir));
        let [north, south, west, east] = connections;
        use RsWireConnection::{None, Side};
        if connections == [None; 4] && self.connections == [None; 4] {
            return Self {
                connections,
                ..self
            };
        }
        if north == None && south == None {
            for connection in &mut connections[2..] {
                if *connection == None {
                    *connection = Side;
                }
            }
        }
        if west == None && east == None {
            for connection in &mut connections[..2] {
                if *connection == None {
                    *connection = Side;
                }
            }
        }
        Self {
            connections,
            ..self
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RedstoneTorchBlock {
    lit: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RedstoneWallTorchBlock {
    // Points away from the attachment face.
    facing: RsDir,
    lit: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RepeaterBlock {
    // Diode facings here point from the rear input toward the front output.
    facing: RsDir,
    delay: u8,
    powered: bool,
    locked: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
impl RepeaterBlock {
    fn delay_ticks(self) -> u64 {
        2 * u64::from(self.delay.clamp(1, 4))
    }

    fn is_locked(self, world: &impl RedstoneWorld, pos: RsPos) -> bool {
        rs_side_input(world, pos, self.facing, true) != 0
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RsComparatorMode {
    Compare,
    Subtract,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ComparatorBlock {
    facing: RsDir,
    mode: RsComparatorMode,
    powered: bool,
    // The analog output is runtime data, separate from the `powered` block-state property.
    output: u8,
}

#[cfg_attr(not(test), allow(dead_code))]
impl ComparatorBlock {
    const DELAY_TICKS: u64 = 2;

    fn evaluate(self, world: &impl RedstoneWorld, pos: RsPos) -> (u8, bool) {
        let rear_pos = pos.relative(self.facing.opposite());
        let mut rear = rs_diode_rear_input(world, pos, self.facing);
        if let Some(analog) = world.container_analog_signal(rear_pos) {
            rear = analog.min(15);
        } else if rear < 15 && world.is_redstone_conductor(rear_pos) {
            let behind = rear_pos.relative(self.facing.opposite());
            if let Some(analog) = world
                .container_analog_signal(behind)
                .max(world.item_frame_signal(behind, self.facing.opposite()))
            {
                rear = analog.min(15);
            }
        }
        let side = rs_side_input(world, pos, self.facing, false);
        let output = match self.mode {
            RsComparatorMode::Compare if rear >= side => rear,
            RsComparatorMode::Compare => 0,
            RsComparatorMode::Subtract => rear.saturating_sub(side),
        };
        let powered =
            rear > side || (rear != 0 && rear == side && self.mode == RsComparatorMode::Compare);
        (output, powered)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RsMountFace {
    Floor,
    Wall,
    Ceiling,
}

impl RsMountFace {
    fn attachment(self, facing: RsDir) -> RsDir {
        match self {
            Self::Floor => RsDir::Down,
            Self::Wall => facing.opposite(),
            Self::Ceiling => RsDir::Up,
        }
    }

    fn from_property(value: &str) -> Option<Self> {
        Some(match value {
            "floor" => Self::Floor,
            "wall" => Self::Wall,
            "ceiling" => Self::Ceiling,
            _ => return None,
        })
    }

    fn property(self) -> &'static str {
        match self {
            Self::Floor => "floor",
            Self::Wall => "wall",
            Self::Ceiling => "ceiling",
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum RsBlock {
    #[default]
    Air,
    Solid,
    Glass,
    Container(u8),
    // Keep unmodelled states intact, including their precise 1.21.1 properties.
    RegistryState(u32),
    RedstoneBlock,
    Lever {
        face: RsMountFace,
        facing: RsDir,
        powered: bool,
    },
    Button {
        block_id: u32,
        face: RsMountFace,
        facing: RsDir,
        powered: bool,
    },
    Observer {
        facing: RsDir,
        powered: bool,
    },
    Target {
        power: u8,
    },
    Piston {
        sticky: bool,
        facing: RsDir,
        extended: bool,
    },
    Source {
        weak: u8,
        strong: u8,
    },
    Wire(RedStoneWireBlock),
    Torch(RedstoneTorchBlock),
    WallTorch(RedstoneWallTorchBlock),
    Repeater(RepeaterBlock),
    Comparator(ComparatorBlock),
    Lamp {
        lit: bool,
    },
    Hopper {
        enabled: bool,
        facing: RsDir,
    },
    Dispenser {
        triggered: bool,
        facing: RsDir,
    },
    Dropper {
        triggered: bool,
        facing: RsDir,
    },
    DaylightDetector {
        inverted: bool,
        power: u8,
    },
    TripwireHook {
        facing: RsDir,
        attached: bool,
        powered: bool,
    },
    SculkSensor {
        calibrated: bool,
        // The amethyst input side; only meaningful on a calibrated sensor.
        facing: RsDir,
        phase: RsSculkPhase,
        power: u8,
        waterlogged: bool,
    },
}

#[cfg_attr(not(test), allow(dead_code))]
impl RsBlock {
    fn diode_facing(self) -> Option<RsDir> {
        match self {
            Self::Repeater(repeater) => Some(repeater.facing),
            Self::Comparator(comparator) => Some(comparator.facing),
            _ => None,
        }
    }

    fn torch_attachment(self) -> Option<(RsDir, bool)> {
        match self {
            Self::Torch(torch) => Some((RsDir::Down, torch.lit)),
            Self::WallTorch(torch) => Some((torch.facing.opposite(), torch.lit)),
            _ => None,
        }
    }

    fn with_torch_lit(self, lit: bool) -> Self {
        match self {
            Self::Torch(_) => Self::Torch(RedstoneTorchBlock { lit }),
            Self::WallTorch(torch) => Self::WallTorch(RedstoneWallTorchBlock { lit, ..torch }),
            _ => self,
        }
    }

    fn with_triggered(self, triggered: bool) -> Self {
        match self {
            Self::Dispenser { facing, .. } => Self::Dispenser { triggered, facing },
            Self::Dropper { facing, .. } => Self::Dropper { triggered, facing },
            _ => self,
        }
    }

    fn is_signal_source(self) -> bool {
        matches!(
            self,
            Self::Wire(_)
                | Self::Torch(_)
                | Self::WallTorch(_)
                | Self::Repeater(_)
                | Self::Comparator(_)
                | Self::RedstoneBlock
                | Self::Lever { .. }
                | Self::Button { .. }
                | Self::Observer { .. }
                | Self::Target { .. }
                | Self::Source { .. }
                | Self::DaylightDetector { .. }
                | Self::TripwireHook { .. }
                | Self::SculkSensor { .. }
        )
    }

    fn weak_signal(self, toward: RsDir) -> u8 {
        match self {
            Self::RedstoneBlock => 15,
            Self::Lever { powered: true, .. } | Self::Button { powered: true, .. } => 15,
            Self::Source { weak, strong } => weak.max(strong).min(15),
            Self::Observer {
                facing,
                powered: true,
            } if toward == facing.opposite() => 15,
            Self::Target { power } => power,
            Self::Wire(wire) => wire.signal(toward),
            Self::Torch(torch) if torch.lit && toward != RsDir::Down => 15,
            Self::WallTorch(torch) if torch.lit && toward != torch.facing.opposite() => 15,
            Self::Repeater(repeater) if repeater.powered && toward == repeater.facing => 15,
            Self::Comparator(comparator) if comparator.powered && toward == comparator.facing => {
                comparator.output.min(15)
            }
            Self::DaylightDetector { power, .. } => power,
            Self::TripwireHook { powered: true, .. } => 15,
            // The calibrated sensor emits nothing on the side opposite its
            // facing: the amethyst input side is a filter, not an output.
            Self::SculkSensor {
                calibrated: true,
                facing,
                ..
            } if toward == facing.opposite() => 0,
            Self::SculkSensor { power, .. } => power,
            _ => 0,
        }
    }

    fn direct_signal(self, toward: RsDir) -> u8 {
        match self {
            Self::Source { strong, .. } => strong.min(15),
            Self::Lever {
                face,
                facing,
                powered: true,
            }
            | Self::Button {
                face,
                facing,
                powered: true,
                ..
            } if toward == face.attachment(facing) => 15,
            Self::Wire(_) | Self::Repeater(_) | Self::Comparator(_) | Self::Observer { .. } => {
                self.weak_signal(toward)
            }
            Self::Torch(_) | Self::WallTorch(_) if toward == RsDir::Up => self.weak_signal(toward),
            // The hook strongly powers the block it is mounted on, opposite the
            // wire line it faces.
            Self::TripwireHook {
                facing,
                powered: true,
                ..
            } if toward == facing.opposite() => 15,
            // Vanilla SculkSensorBlock.getDirectSignal: the sensor strongly
            // powers only the block below it.
            Self::SculkSensor { .. } if toward == RsDir::Down => self.weak_signal(toward),
            _ => 0,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
trait RedstoneWorld {
    fn redstone_block(&self, pos: RsPos) -> RsBlock;
    fn is_redstone_conductor(&self, pos: RsPos) -> bool;

    fn is_face_sturdy(&self, pos: RsPos, _face: RsDir) -> bool {
        self.is_redstone_conductor(pos)
    }

    fn supports_wire_on_top(&self, pos: RsPos) -> bool {
        self.is_face_sturdy(pos, RsDir::Up)
            || matches!(self.redstone_block(pos), RsBlock::Hopper { .. })
    }

    fn allows_wire_step_up(&self, pos: RsPos) -> bool {
        self.supports_wire_on_top(pos)
    }

    fn item_frame_signal(&self, _pos: RsPos, _facing: RsDir) -> Option<u8> {
        None
    }

    fn container_analog_signal(&self, pos: RsPos) -> Option<u8> {
        match self.redstone_block(pos) {
            RsBlock::Container(signal) => Some(signal),
            _ => None,
        }
    }

    /// `(sky_light, day_time, raining, thundering)` for a daylight detector's
    /// beam, or `None` when the host cannot observe the sky there. `None` keeps
    /// the block's stored `power` property authoritative.
    fn daylight_env(&self, _pos: RsPos) -> Option<(u8, i64, bool, bool)> {
        None
    }

    /// Walk the tripwire line starting at the hook's `facing`: wire cells and
    /// the far hook, folded into the vanilla attach/power result, or `None`
    /// when the host cannot report entity intersection.
    fn tripwire_line(&self, _pos: RsPos, _facing: RsDir) -> Option<RsTripwireResult> {
        None
    }

    /// The vibration a sculk sensor at `pos` would hear, or `None`. The host
    /// owns event delivery and wool occlusion; the block model owns power math.
    fn sculk_vibration(&self, _pos: RsPos) -> Option<RsSculkVibration> {
        None
    }
}

/// One game event reaching a sculk sensor: the vanilla game-event name, the
/// Euclidean distance to the source and whether wool blocked the path.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug)]
struct RsSculkVibration {
    event: String,
    distance: f64,
    wool_occluded: bool,
}

// These evaluators intentionally take the environmental observations as inputs. The
// world adapter owns storage and event/entity queries; keeping the math here makes
// the 1.21.1 state transitions deterministic and independently testable.
#[cfg_attr(not(test), allow(dead_code))]
fn rs_daylight_detector_signal(
    sky_light: u8,
    day_time: i64,
    raining: bool,
    thundering: bool,
    inverted: bool,
) -> u8 {
    let time = day_time.rem_euclid(24_000) as f32 / 24_000.0 - 0.25;
    let time = time.rem_euclid(1.0);
    let eased_time = time + (0.5 - (std::f32::consts::PI * time).cos() / 2.0 - time) / 3.0;
    let sun = ((eased_time * std::f32::consts::TAU).cos() * 2.0 + 0.2).clamp(0.0, 1.0);
    let mut signal = (sun * 15.0 + 0.5).floor() as u8;
    if raining {
        signal = (f32::from(signal) * 0.9) as u8;
    }
    if thundering {
        signal = (f32::from(signal) * 0.9) as u8;
    }
    let sky_darken = (((1.0 - sun) * 11.0).floor() as u8
        + if raining { 5 } else { 0 }
        + if thundering { 5 } else { 0 })
    .min(15);
    signal = signal.max(sky_light.saturating_sub(sky_darken));
    if inverted {
        15 - signal.min(15)
    } else {
        signal.min(15)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RsTripwireCell {
    Wire {
        powered: bool,
        disarmed: bool,
        entity_intersects: bool,
    },
    Hook {
        facing: RsDir,
        attached: bool,
    },
    Other,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RsTripwireWireResult {
    attached: bool,
    // The entity-driven powered flag the cell's own tick would write, not the
    // stored one it read; writing it back keeps `powered` from latching.
    powered: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq)]
struct RsTripwireResult {
    attached: bool,
    powered: bool,
    wire_count: u8,
    wires: Vec<RsTripwireWireResult>,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq)]
struct RsAabb {
    min: [f64; 3],
    max: [f64; 3],
}

#[cfg_attr(not(test), allow(dead_code))]
fn rs_tripwire_entity_intersects(wire: RsAabb, entities: &[RsAabb]) -> bool {
    entities.iter().any(|entity| {
        (0..3).all(|axis| wire.min[axis] < entity.max[axis] && wire.max[axis] > entity.min[axis])
    })
}

/// `cells` starts immediately beyond the near hook and advances along its facing.
/// At most 40 wire blocks may occur before the opposing hook. Each wire input carries
/// the result of the entity AABB intersection check for that wire.
#[cfg_attr(not(test), allow(dead_code))]
fn rs_tripwire_line(facing: RsDir, cells: &[RsTripwireCell]) -> RsTripwireResult {
    let mut wire_states = Vec::new();
    let mut powered = false;
    for cell in cells.iter().take(41) {
        match *cell {
            RsTripwireCell::Wire {
                powered: wire_powered,
                disarmed,
                entity_intersects,
            } if wire_states.len() < 40 => {
                powered |= wire_powered || (entity_intersects && !disarmed);
                wire_states.push(RsTripwireWireResult {
                    attached: false,
                    powered: entity_intersects && !disarmed,
                });
            }
            RsTripwireCell::Hook {
                facing: other_facing,
                attached,
            } => {
                let connected =
                    other_facing == facing.opposite() && attached && !wire_states.is_empty();
                for wire in &mut wire_states {
                    wire.attached = connected;
                }
                return RsTripwireResult {
                    attached: connected,
                    powered: connected && powered,
                    wire_count: wire_states.len() as u8,
                    wires: wire_states,
                };
            }
            _ => {
                return RsTripwireResult {
                    attached: false,
                    powered: false,
                    wire_count: wire_states.len() as u8,
                    wires: wire_states,
                };
            }
        }
    }
    RsTripwireResult {
        attached: false,
        powered: false,
        wire_count: wire_states.len() as u8,
        wires: wire_states,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn rs_sculk_frequency(event: &str) -> Option<u8> {
    Some(match event.strip_prefix("minecraft:").unwrap_or(event) {
        "step" | "swim" | "flap" => 1,
        "projectile_land" | "hit_ground" => 2,
        "splash" | "fluid_pickup" | "fluid_place" => 3,
        "item_interact_start" | "item_interact_finish" | "projectile_shoot" => 4,
        "block_destroy" => 5,
        "dispense_fail" => 6,
        "block_place" | "entity_place" => 7,
        "entity_damage" => 8,
        "eat" | "drink" => 9,
        "container_open" | "container_close" => 10,
        "block_open" | "block_close" | "block_activate" | "block_deactivate" | "block_attach"
        | "block_detach" | "block_change" => 11,
        "entity_roar" | "entity_shake" | "equip" | "unequip" => 12,
        "entity_die" | "teleport" => 13,
        "explode" => 14,
        "lightning_strike" => 15,
        _ => return None,
    })
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RsSculkPhase {
    Inactive,
    Active,
    Cooldown,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RsSculkEvaluation {
    phase: RsSculkPhase,
    power: u8,
}

/// Compute event power and the elapsed-tick phase. `distance` is Euclidean block
/// distance; the native listener rejects wool-occluded events before this stage.
#[cfg_attr(not(test), allow(dead_code))]
fn rs_sculk_sensor_evaluate(
    event: &str,
    distance: f64,
    range: u8,
    wool_occluded: bool,
    calibrated_frequency: Option<u8>,
    calibrated: bool,
    elapsed_ticks: Option<u8>,
) -> RsSculkEvaluation {
    let frequency = rs_sculk_frequency(event).unwrap_or(0);
    let in_range = !wool_occluded && range > 0 && distance <= f64::from(range);
    let matches_calibration = !calibrated || calibrated_frequency == Some(frequency);
    let power = if in_range && matches_calibration && frequency > 0 {
        15 - ((distance * 15.0 / f64::from(range)).floor() as u8).min(15)
    } else {
        0
    };
    let active_ticks = if calibrated { 10 } else { 30 };
    let phase = match elapsed_ticks {
        Some(ticks) if ticks < active_ticks => RsSculkPhase::Active,
        Some(ticks) if ticks < active_ticks + 10 => RsSculkPhase::Cooldown,
        _ => RsSculkPhase::Inactive,
    };
    RsSculkEvaluation {
        phase,
        power: if phase == RsSculkPhase::Active {
            power
        } else {
            0
        },
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn rs_signal_source(
    world: &impl RedstoneWorld,
    pos: RsPos,
    toward: RsDir,
    include_wire: bool,
) -> RsBlock {
    match world.redstone_block(pos) {
        RsBlock::Wire(_) if !include_wire => RsBlock::Air,
        // Vanilla samples current geometry for horizontal dust output.
        RsBlock::Wire(wire) if wire.power != 0 && toward.horizontal_index().is_some() => {
            RsBlock::Wire(wire.with_connections(world, pos))
        }
        block => block,
    }
}

// Signal directions point from the emitting block toward the receiver.
#[cfg_attr(not(test), allow(dead_code))]
fn rs_signal_from(world: &impl RedstoneWorld, pos: RsPos, toward: RsDir, include_wire: bool) -> u8 {
    let weak = rs_signal_source(world, pos, toward, include_wire).weak_signal(toward);
    if weak == 15 || !world.is_redstone_conductor(pos) {
        return weak;
    }

    // A conductor receives strong power from one neighboring source and emits
    // an effective signal. The lookup never walks through a second conductor.
    let mut signal = weak;
    for dir in RsDir::ALL {
        let toward = dir.opposite();
        let source = rs_signal_source(world, pos.relative(dir), toward, include_wire);
        signal = signal.max(source.direct_signal(toward));
        if signal == 15 {
            break;
        }
    }
    signal
}

#[cfg_attr(not(test), allow(dead_code))]
fn rs_neighbor_signal(world: &impl RedstoneWorld, pos: RsPos, include_wire: bool) -> u8 {
    RsDir::ALL
        .into_iter()
        .map(|dir| rs_signal_from(world, pos.relative(dir), dir.opposite(), include_wire))
        .max()
        .unwrap_or(0)
}

#[cfg_attr(not(test), allow(dead_code))]
fn rs_wire_connection(world: &impl RedstoneWorld, pos: RsPos, dir: RsDir) -> RsWireConnection {
    let side = pos.relative(dir);
    let block = world.redstone_block(side);
    if !world.is_redstone_conductor(pos.above())
        && world.allows_wire_step_up(side)
        && matches!(world.redstone_block(side.above()), RsBlock::Wire(_))
    {
        return if world.is_face_sturdy(side, dir.opposite()) {
            RsWireConnection::Up
        } else {
            RsWireConnection::Side
        };
    }

    let connects = match block {
        RsBlock::Repeater(repeater) => repeater.facing == dir || repeater.facing == dir.opposite(),
        RsBlock::Observer { facing, .. } => facing == dir,
        _ => block.is_signal_source(),
    };
    if connects
        || (!world.is_redstone_conductor(side)
            && matches!(
                world.redstone_block(side.relative(RsDir::Down)),
                RsBlock::Wire(_)
            ))
    {
        RsWireConnection::Side
    } else {
        RsWireConnection::None
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn rs_wire_inputs(world: &impl RedstoneWorld, pos: RsPos) -> impl Iterator<Item = RsPos> + '_ {
    let can_step_up = !world.is_redstone_conductor(pos.above());
    RsDir::HORIZONTAL.into_iter().flat_map(move |dir| {
        let side = pos.relative(dir);
        let stepped = if world.is_redstone_conductor(side) {
            can_step_up.then(|| side.above())
        } else {
            Some(side.relative(RsDir::Down))
        };
        [Some(side), stepped].into_iter().flatten()
    })
}

#[cfg_attr(not(test), allow(dead_code))]
fn rs_diode_rear_input(world: &impl RedstoneWorld, pos: RsPos, facing: RsDir) -> u8 {
    let rear = pos.relative(facing.opposite());
    let signal = rs_signal_from(world, rear, facing, true);
    match world.redstone_block(rear) {
        RsBlock::Wire(wire) => signal.max(wire.power),
        _ => signal,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn rs_side_input(world: &impl RedstoneWorld, pos: RsPos, facing: RsDir, diodes_only: bool) -> u8 {
    [facing.left(), facing.right()]
        .into_iter()
        .map(|dir| {
            let block = world.redstone_block(pos.relative(dir));
            let toward = dir.opposite();
            if diodes_only {
                return if block.diode_facing().is_some() {
                    block.direct_signal(toward)
                } else {
                    0
                };
            }
            match block {
                RsBlock::Wire(wire) => wire.power,
                RsBlock::RedstoneBlock => 15,
                _ => block.direct_signal(toward),
            }
        })
        .max()
        .unwrap_or(0)
}

#[cfg_attr(not(test), allow(dead_code))]
fn container_analog_output_signal(slots: &[(u32, u32)], num_slots: usize) -> u8 {
    if num_slots == 0 {
        return 0;
    }
    let fullness: f32 = slots
        .iter()
        .take(num_slots)
        .filter(|&&(count, max_stack)| count != 0 && max_stack != 0)
        .map(|&(count, max_stack)| count as f32 / max_stack as f32)
        .sum();
    if fullness == 0.0 {
        return 0;
    }
    (1.0 + (14.0 * (fullness / num_slots as f32)).floor()).min(15.0) as u8
}

#[cfg_attr(not(test), allow(dead_code))]
fn is_piston_powered(world: &impl RedstoneWorld, pos: RsPos, facing: RsDir) -> bool {
    RsDir::ALL.into_iter().any(|dir| {
        dir != facing && rs_signal_from(world, pos.relative(dir), dir.opposite(), true) != 0
    }) || rs_signal_from(world, pos, RsDir::Up, true) != 0
        || RsDir::ALL.into_iter().any(|dir| {
            dir != RsDir::Down
                && rs_signal_from(world, pos.above().relative(dir), dir.opposite(), true) != 0
        })
}

#[cfg_attr(not(test), allow(dead_code))]
fn rs_dispenser_input(world: &impl RedstoneWorld, pos: RsPos) -> bool {
    rs_neighbor_signal(world, pos, true) != 0 || rs_neighbor_signal(world, pos.above(), true) != 0
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(i8)]
enum RsTickPriority {
    ExtremelyHigh = -3,
    VeryHigh = -2,
    High = -1,
    Normal = 0,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Default)]
struct RsTorchHistory {
    shutoffs: VecDeque<u64>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl RsTorchHistory {
    const DELAY: u64 = 2;
    const BURNOUT_WINDOW: u64 = 60;
    const BURNOUT_THRESHOLD: usize = 8;
    const COOLDOWN: u64 = 160;

    fn record_shutoff(&mut self, now: u64) -> bool {
        // Mojang's toggle log records lit-to-unlit updates, including the 60 GT boundary.
        while self
            .shutoffs
            .front()
            .is_some_and(|&tick| now - tick > Self::BURNOUT_WINDOW)
        {
            self.shutoffs.pop_front();
        }
        self.shutoffs.push_back(now);
        self.shutoffs.len() >= Self::BURNOUT_THRESHOLD
    }

    fn is_burned_out(&self, now: u64) -> bool {
        self.shutoffs
            .iter()
            .rev()
            .nth(Self::BURNOUT_THRESHOLD - 1)
            .is_some_and(|&tick| now - tick <= Self::BURNOUT_WINDOW)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
trait RedstoneUpdates: RedstoneWorld {
    fn set_component(&mut self, pos: RsPos, block: RsBlock);
    fn schedule(&mut self, pos: RsPos, delay: u64, priority: RsTickPriority);
    fn torch_histories(&mut self) -> &mut BTreeMap<RsPos, RsTorchHistory>;
    fn dispense(&mut self, pos: RsPos);
    /// Write a tripwire cell's `attached`/`powered` properties. Hosts that let
    /// the entity tick own those flags keep the default no-op.
    fn update_tripwire_cell(&mut self, _pos: RsPos, _wire: RsTripwireWireResult) {}
}

#[cfg_attr(not(test), allow(dead_code))]
type RsScheduledTick = Reverse<(u64, RsTickPriority, u64, RsPos)>;

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Default)]
struct RedstoneSimGrid {
    blocks: BTreeMap<RsPos, RsBlock>,
    now: u64,
    next_sequence: u64,
    ticks: BinaryHeap<RsScheduledTick>,
    pending: BTreeMap<RsPos, u64>,
    torches: BTreeMap<RsPos, RsTorchHistory>,
    dispense_events: Vec<RsPos>,
}

impl RedstoneWorld for RedstoneSimGrid {
    fn redstone_block(&self, pos: RsPos) -> RsBlock {
        self.blocks.get(&pos).copied().unwrap_or_default()
    }

    fn is_redstone_conductor(&self, pos: RsPos) -> bool {
        matches!(
            self.redstone_block(pos),
            RsBlock::Solid
                | RsBlock::Lamp { .. }
                | RsBlock::Dispenser { .. }
                | RsBlock::Dropper { .. }
        )
    }

    fn is_face_sturdy(&self, pos: RsPos, _face: RsDir) -> bool {
        self.is_redstone_conductor(pos)
            || matches!(
                self.redstone_block(pos),
                RsBlock::Glass | RsBlock::RedstoneBlock
            )
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl RedstoneSimGrid {
    fn set_block(&mut self, pos: RsPos, block: RsBlock) {
        if std::mem::discriminant(&self.redstone_block(pos)) != std::mem::discriminant(&block) {
            self.pending.remove(&pos);
            self.torches.remove(&pos);
        }
        if block == RsBlock::Air {
            self.blocks.remove(&pos);
        } else {
            self.blocks.insert(pos, block);
        }
        self.settle();
    }

    fn advance(&mut self, ticks: u64) {
        let target = self.now + ticks;
        while let Some(&Reverse((due, _, sequence, pos))) = self.ticks.peek() {
            if due > target {
                break;
            }
            self.ticks.pop();
            if self.pending.get(&pos) != Some(&sequence) {
                continue;
            }
            self.pending.remove(&pos);
            self.now = due;
            rs_run_tick(self, pos, due);
            self.settle();
        }
        self.now = target;
    }

    fn take_dispense_events(&mut self) -> Vec<RsPos> {
        std::mem::take(&mut self.dispense_events)
    }

    fn settle(&mut self) {
        let positions: Vec<_> = self.blocks.keys().copied().collect();
        rs_settle_wires(self, positions.iter().copied());
        for pos in positions {
            let block = self.redstone_block(pos);
            rs_update_device(self, pos, block);
        }
    }
}

impl RedstoneUpdates for RedstoneSimGrid {
    fn set_component(&mut self, pos: RsPos, block: RsBlock) {
        self.blocks.insert(pos, block);
    }

    fn schedule(&mut self, pos: RsPos, delay: u64, priority: RsTickPriority) {
        if let std::collections::btree_map::Entry::Vacant(entry) = self.pending.entry(pos) {
            let sequence = self.next_sequence;
            self.next_sequence += 1;
            entry.insert(sequence);
            self.ticks
                .push(Reverse((self.now + delay, priority, sequence, pos)));
        }
    }

    fn torch_histories(&mut self) -> &mut BTreeMap<RsPos, RsTorchHistory> {
        &mut self.torches
    }

    fn dispense(&mut self, pos: RsPos) {
        self.dispense_events.push(pos);
    }
}

// Seed with non-wire power only. Relaxing from zero clears disconnected cycles
// and computes the final power before writing any wire to the host.
#[cfg_attr(not(test), allow(dead_code))]
fn rs_settle_wires(world: &mut impl RedstoneUpdates, positions: impl IntoIterator<Item = RsPos>) {
    let mut wires = BTreeMap::new();
    let mut downstream: BTreeMap<RsPos, Vec<RsPos>> = BTreeMap::new();
    let mut queue = VecDeque::new();
    for pos in positions {
        let RsBlock::Wire(wire) = world.redstone_block(pos) else {
            continue;
        };
        let mut updated = wire.with_connections(world, pos);
        updated.power = rs_neighbor_signal(world, pos, false);
        if updated.power > 1 {
            queue.push_back(pos);
        }
        wires.insert(pos, (wire, updated));
        for input in rs_wire_inputs(world, pos) {
            if matches!(world.redstone_block(input), RsBlock::Wire(_)) {
                downstream.entry(input).or_default().push(pos);
            }
        }
    }

    while let Some(pos) = queue.pop_front() {
        let power = wires[&pos].1.power.saturating_sub(1);
        if let Some(receivers) = downstream.get(&pos) {
            for &receiver in receivers {
                let (_, wire) = wires.get_mut(&receiver).expect("wire receiver was indexed");
                if power > wire.power {
                    wire.power = power;
                    if power > 1 {
                        queue.push_back(receiver);
                    }
                }
            }
        }
    }

    for (pos, (wire, updated)) in wires {
        if updated != wire {
            world.set_component(pos, RsBlock::Wire(updated));
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn rs_diode_in_front(world: &impl RedstoneWorld, pos: RsPos, facing: RsDir) -> bool {
    world
        .redstone_block(pos.relative(facing))
        .diode_facing()
        .is_some_and(|other| other != facing.opposite())
}

#[cfg_attr(not(test), allow(dead_code))]
fn rs_update_device(world: &mut impl RedstoneUpdates, pos: RsPos, block: RsBlock) {
    if let Some((attachment, lit)) = block.torch_attachment() {
        let powered =
            rs_signal_from(world, pos.relative(attachment), attachment.opposite(), true) != 0;
        if powered == lit {
            world.schedule(pos, RsTorchHistory::DELAY, RsTickPriority::Normal);
        }
        return;
    }

    let updated = match block {
        RsBlock::Repeater(mut repeater) => {
            repeater.locked = repeater.is_locked(world, pos);
            let powered = rs_diode_rear_input(world, pos, repeater.facing) != 0;
            if !repeater.locked && repeater.powered != powered {
                let priority = if rs_diode_in_front(world, pos, repeater.facing) {
                    RsTickPriority::ExtremelyHigh
                } else if repeater.powered {
                    RsTickPriority::VeryHigh
                } else {
                    RsTickPriority::High
                };
                world.schedule(pos, repeater.delay_ticks(), priority);
            }
            RsBlock::Repeater(repeater)
        }
        RsBlock::Comparator(comparator) => {
            if (comparator.output, comparator.powered) != comparator.evaluate(world, pos) {
                let priority = if rs_diode_in_front(world, pos, comparator.facing) {
                    RsTickPriority::High
                } else {
                    RsTickPriority::Normal
                };
                world.schedule(pos, ComparatorBlock::DELAY_TICKS, priority);
            }
            block
        }
        RsBlock::Lamp { lit } => {
            let powered = rs_neighbor_signal(world, pos, true) != 0;
            if lit && !powered {
                world.schedule(pos, 4, RsTickPriority::Normal);
            }
            RsBlock::Lamp {
                lit: lit || powered,
            }
        }
        RsBlock::Hopper { facing, .. } => RsBlock::Hopper {
            enabled: rs_neighbor_signal(world, pos, true) == 0,
            facing,
        },
        RsBlock::Dispenser { triggered, .. } | RsBlock::Dropper { triggered, .. } => {
            let powered = rs_dispenser_input(world, pos);
            if !triggered && powered {
                world.schedule(pos, 4, RsTickPriority::Normal);
            }
            block.with_triggered(powered)
        }
        RsBlock::DaylightDetector { inverted, .. } => rs_daylight_eval(world, pos, inverted, block),
        RsBlock::TripwireHook { .. } => rs_tripwire_hook_update(world, pos, block),
        RsBlock::SculkSensor {
            calibrated,
            facing,
            phase: RsSculkPhase::Inactive,
            waterlogged,
            ..
        } => {
            let Some(vibration) = world.sculk_vibration(pos) else {
                return;
            };
            let range = if calibrated { 16 } else { 8 };
            // Vanilla getFacingInputSignal: the front block's signal, including
            // an explicit dust power read when the wire is not pointed back.
            let filter = calibrated
                .then(|| {
                    let front = pos.relative(facing);
                    let input = rs_signal_from(world, front, facing.opposite(), true)
                        .max(match world.redstone_block(front) {
                            RsBlock::Wire(wire) => wire.power,
                            _ => 0,
                        });
                    (input > 0).then_some(input)
                })
                .flatten();
            let evaluation = rs_sculk_sensor_evaluate(
                &vibration.event,
                vibration.distance,
                range,
                vibration.wool_occluded,
                filter,
                calibrated,
                Some(0),
            );
            if evaluation.power > 0 {
                world.schedule(
                    pos,
                    if calibrated { 10 } else { 30 },
                    RsTickPriority::Normal,
                );
                RsBlock::SculkSensor {
                    calibrated,
                    facing,
                    phase: evaluation.phase,
                    power: evaluation.power,
                    waterlogged,
                }
            } else {
                block
            }
        }
        _ => block,
    };
    if updated != block {
        world.set_component(pos, updated);
    }
}

/// Re-read the environment for a daylight detector and reschedule the next
/// refresh; `None` env keeps the stored `power` like a vanilla unloaded beam.
#[cfg_attr(not(test), allow(dead_code))]
fn rs_daylight_eval(
    world: &mut impl RedstoneUpdates,
    pos: RsPos,
    inverted: bool,
    block: RsBlock,
) -> RsBlock {
    let Some((sky_light, day_time, raining, thundering)) = world.daylight_env(pos) else {
        return block;
    };
    world.schedule(pos, 1, RsTickPriority::Normal);
    RsBlock::DaylightDetector {
        inverted,
        power: rs_daylight_detector_signal(sky_light, day_time, raining, thundering, inverted),
    }
}

/// Rescan the line the hook faces, pushing `attached`/`powered` back into the
/// wire cells before reporting the hook's own flags.
#[cfg_attr(not(test), allow(dead_code))]
fn rs_tripwire_hook_update(world: &mut impl RedstoneUpdates, pos: RsPos, block: RsBlock) -> RsBlock {
    let RsBlock::TripwireHook { facing, .. } = block else {
        return block;
    };
    let Some(result) = world.tripwire_line(pos, facing) else {
        return block;
    };
    let mut cell_pos = pos;
    for wire in &result.wires {
        cell_pos = cell_pos.relative(facing);
        world.update_tripwire_cell(cell_pos, *wire);
    }
    RsBlock::TripwireHook {
        facing,
        attached: result.attached,
        powered: result.powered,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn rs_run_tick(world: &mut impl RedstoneUpdates, pos: RsPos, now: u64) {
    let block = world.redstone_block(pos);
    if let Some((attachment, lit)) = block.torch_attachment() {
        let powered =
            rs_signal_from(world, pos.relative(attachment), attachment.opposite(), true) != 0;
        if lit && powered {
            if world
                .torch_histories()
                .entry(pos)
                .or_default()
                .record_shutoff(now)
            {
                world.schedule(pos, RsTorchHistory::COOLDOWN, RsTickPriority::Normal);
            }
            world.set_component(pos, block.with_torch_lit(false));
        } else if !lit
            && !powered
            && !world
                .torch_histories()
                .get(&pos)
                .is_some_and(|history| history.is_burned_out(now))
        {
            world.set_component(pos, block.with_torch_lit(true));
        }
        return;
    }

    if matches!(
        block,
        RsBlock::DaylightDetector { .. } | RsBlock::TripwireHook { .. }
    ) {
        // These sensors poll their environment; the scheduled tick is just the
        // next refresh of the same neighbor-change evaluation.
        rs_update_device(world, pos, block);
        return;
    }

    let updated = match block {
        RsBlock::Repeater(mut repeater) if !repeater.is_locked(world, pos) => {
            let input = rs_diode_rear_input(world, pos, repeater.facing) != 0;
            if !repeater.powered && !input {
                // A queued rising edge still turns on after the input falls.
                world.schedule(pos, repeater.delay_ticks(), RsTickPriority::VeryHigh);
            }
            repeater.powered = !repeater.powered || input;
            RsBlock::Repeater(repeater)
        }
        RsBlock::Comparator(mut comparator) => {
            let (output, powered) = comparator.evaluate(world, pos);
            // In subtract mode a refresh only updates POWERED if the output changed.
            if output != comparator.output || comparator.mode == RsComparatorMode::Compare {
                comparator.powered = powered;
                comparator.output = output;
                world.set_component(pos, RsBlock::Comparator(comparator));
            }
            return;
        }
        RsBlock::Lamp { lit: true } if rs_neighbor_signal(world, pos, true) == 0 => {
            RsBlock::Lamp { lit: false }
        }
        RsBlock::SculkSensor {
            calibrated,
            facing,
            phase: RsSculkPhase::Active,
            waterlogged,
            ..
        } => {
            world.schedule(pos, 10, RsTickPriority::Normal);
            RsBlock::SculkSensor {
                calibrated,
                facing,
                phase: RsSculkPhase::Cooldown,
                power: 0,
                waterlogged,
            }
        }
        RsBlock::SculkSensor {
            calibrated,
            facing,
            phase: RsSculkPhase::Cooldown,
            waterlogged,
            ..
        } => RsBlock::SculkSensor {
            calibrated,
            facing,
            phase: RsSculkPhase::Inactive,
            power: 0,
            waterlogged,
        },
        RsBlock::Dispenser { .. } | RsBlock::Dropper { .. } => {
            world.dispense(pos);
            block
        }
        _ => block,
    };
    if updated != block {
        world.set_component(pos, updated);
    }
}

#[cfg(test)]
mod redstone_tests {
    use super::*;
    use crate::json::jparse;

    const ORIGIN: RsPos = RsPos(0, 0, 0);

    fn registered(reg: &BReg, name: &str, properties: &[(&str, &str)]) -> u32 {
        let properties = properties
            .iter()
            .map(|&(key, value)| (key.into(), value.into()))
            .collect::<Vec<_>>();
        reg.checked_state(name, &properties)
            .unwrap_or_else(|| panic!("missing 1.21.1 state {name} {properties:?}"))
    }

    #[test]
    fn registry_codec_round_trips_every_supported_1211_state() {
        let reg = breg().unwrap();
        let asset = std::fs::read("assets/block_states_1211.json").unwrap();
        let states = jparse(&asset).unwrap();
        let mut count = 0;
        for entry in states.get("blocks").unwrap().as_arr().unwrap() {
            let row = entry.as_arr().unwrap();
            let name = row[0].str();
            let name = name.strip_prefix("minecraft:").unwrap_or(name);
            let modeled = matches!(
                name,
                "air"
                    | "redstone_wire"
                    | "repeater"
                    | "comparator"
                    | "redstone_torch"
                    | "redstone_wall_torch"
                    | "lever"
                    | "observer"
                    | "target"
                    | "redstone_block"
                    | "redstone_lamp"
                    | "hopper"
                    | "piston"
                    | "sticky_piston"
                    | "dispenser"
                    | "dropper"
                    | "daylight_detector"
                    | "tripwire_hook"
                    | "sculk_sensor"
                    | "calibrated_sculk_sensor"
            ) || name.ends_with("_button");
            let start = row[1].num() as u32;
            let len = row[2].num() as u32;
            if name == "redstone_wire" {
                assert_eq!(len, 16 * 3u32.pow(4), "wire power and connection states");
            }
            for state in start..start + len {
                let block = rs_block_from_state(reg, state);
                assert_eq!(
                    !matches!(block, RsBlock::RegistryState(_)),
                    modeled,
                    "{name} state {state}"
                );
                assert_eq!(
                    state_from_rs_block(reg, &block),
                    Some(state),
                    "{name} state {state}"
                );
                count += 1;
            }
        }
        assert_eq!(count, reg.state_count());
        let invalid = rs_block_from_state(reg, u32::MAX);
        assert_eq!(invalid, RsBlock::RegistryState(u32::MAX));
        assert_eq!(state_from_rs_block(reg, &invalid), None);
        assert_eq!(
            state_from_rs_block(
                reg,
                &RsBlock::Wire(RedStoneWireBlock {
                    power: 16,
                    ..RedStoneWireBlock::default()
                })
            ),
            None
        );
    }

    #[test]
    fn registry_conductor_and_support_use_native_state_flags() {
        let reg = breg().unwrap();
        let mut world = MemoryWorld::new(reg);
        let mut torches = BTreeMap::new();
        world.states.extend([
            (RsPos(0, 0, 0), registered(reg, "stone", &[])),
            (RsPos(1, 0, 0), registered(reg, "glass", &[])),
            (
                RsPos(3, 0, 0),
                registered(reg, "oak_slab", &[("type", "bottom")]),
            ),
        ]);
        let adapter = RedstoneWorldAdapter::new(reg, &mut world, &mut torches);
        assert!(adapter.is_redstone_conductor(ORIGIN));
        for x in 1..=3 {
            assert!(!adapter.is_redstone_conductor(RsPos(x, 0, 0)), "x={x}");
        }
        assert!(adapter.is_face_sturdy(ORIGIN, RsDir::Up));
        assert!(adapter.is_face_sturdy(RsPos(1, 0, 0), RsDir::Up));
        assert!(!adapter.is_face_sturdy(RsPos(2, 0, 0), RsDir::Up));
    }

    #[test]
    fn registry_states_drive_torch_wire_repeater_lamp_circuit() {
        let reg = breg().unwrap();
        let mut world = MemoryWorld::new(reg);
        let mut torches = BTreeMap::new();
        let [torch, wire, repeater, lamp] = [0, 1, 2, 3].map(|x| RsPos(x, 1, 0));
        let stone = registered(reg, "stone", &[]);
        world.states.extend([
            (ORIGIN, stone),
            (RsPos(1, 0, 0), stone),
            (RsPos(2, 0, 0), stone),
            (torch, registered(reg, "redstone_torch", &[("lit", "true")])),
            (wire, registered(reg, "redstone_wire", &[("power", "0")])),
            (
                repeater,
                registered(
                    reg,
                    "repeater",
                    &[("facing", "west"), ("delay", "1"), ("powered", "false")],
                ),
            ),
            (lamp, registered(reg, "redstone_lamp", &[("lit", "false")])),
        ]);
        let prop = |world: &MemoryWorld, pos, key| reg.get_prop(world.state_at(pos), key);

        RedstoneWorldAdapter::new(reg, &mut world, &mut torches)
            .propagate(&[torch, wire, repeater, lamp]);
        assert_eq!(prop(&world, wire, "power").as_deref(), Some("15"));
        assert_eq!(prop(&world, repeater, "powered").as_deref(), Some("false"));
        assert!(world
            .pending
            .iter()
            .any(|&(due, tick)| due == 2 && tick.pos == repeater));

        world.advance(2, &mut torches);
        assert_eq!(prop(&world, repeater, "powered").as_deref(), Some("true"));
        assert_eq!(prop(&world, lamp, "lit").as_deref(), Some("true"));

        world.advance(3, &mut torches);
        let source = RsPos(-1, 0, 0);
        world.states.insert(
            source,
            registered(
                reg,
                "lever",
                &[("face", "wall"), ("facing", "west"), ("powered", "true")],
            ),
        );
        RedstoneWorldAdapter::new(reg, &mut world, &mut torches).propagate(&[source]);
        assert!(world
            .pending
            .iter()
            .any(|&(due, tick)| due == 5 && tick.pos == torch));
        world.advance(5, &mut torches);
        assert_eq!(prop(&world, torch, "lit").as_deref(), Some("false"));
        assert_eq!(prop(&world, wire, "power").as_deref(), Some("0"));
        world.advance(7, &mut torches);
        assert_eq!(prop(&world, repeater, "powered").as_deref(), Some("false"));
        assert_eq!(prop(&world, lamp, "lit").as_deref(), Some("true"));
        world.advance(11, &mut torches);
        assert_eq!(prop(&world, lamp, "lit").as_deref(), Some("false"));
    }

    fn wire() -> RsBlock {
        RsBlock::Wire(RedStoneWireBlock::default())
    }

    fn dust(grid: &RedstoneSimGrid, pos: RsPos) -> RedStoneWireBlock {
        let RsBlock::Wire(wire) = grid.redstone_block(pos) else {
            panic!("no wire at {pos:?}");
        };
        wire
    }

    fn repeater(facing: RsDir, delay: u8) -> RsBlock {
        RsBlock::Repeater(RepeaterBlock {
            facing,
            delay,
            powered: false,
            locked: false,
        })
    }

    fn repeater_state(grid: &RedstoneSimGrid, pos: RsPos) -> (bool, bool) {
        let RsBlock::Repeater(repeater) = grid.redstone_block(pos) else {
            panic!("no repeater at {pos:?}");
        };
        (repeater.powered, repeater.locked)
    }

    fn comparator(facing: RsDir, mode: RsComparatorMode, output: u8) -> RsBlock {
        RsBlock::Comparator(ComparatorBlock {
            facing,
            mode,
            powered: output > 0,
            output,
        })
    }

    #[test]
    fn wire_decays_to_zero_and_clears_after_source_removal() {
        let mut grid = RedstoneSimGrid::default();
        for x in (1..=16).rev() {
            grid.set_block(RsPos(x, 0, 0), wire());
        }

        grid.set_block(ORIGIN, RsBlock::RedstoneBlock);
        let levels: Vec<_> = (1..=16)
            .map(|x| dust(&grid, RsPos(x, 0, 0)).power)
            .collect();
        assert_eq!(levels, (0..=15).rev().collect::<Vec<_>>());

        let first_wire = grid.redstone_block(RsPos(1, 0, 0));
        assert_eq!(first_wire.weak_signal(RsDir::Down), 15);
        assert_eq!(first_wire.weak_signal(RsDir::Up), 0);
        assert_eq!(first_wire.weak_signal(RsDir::East), 15);
        assert_eq!(first_wire.weak_signal(RsDir::North), 0);

        grid.set_block(ORIGIN, RsBlock::Air);
        for x in 1..=16 {
            assert_eq!(dust(&grid, RsPos(x, 0, 0)).power, 0, "wire {x}");
        }
    }

    #[test]
    fn strong_power_crosses_one_conductor_but_weak_power_does_not() {
        let mut grid = RedstoneSimGrid::default();
        let source = RsPos(-1, 0, 0);
        let output = RsPos(1, 0, 0);
        grid.set_block(ORIGIN, RsBlock::Solid);
        grid.set_block(output, wire());
        grid.set_block(
            source,
            RsBlock::Source {
                weak: 15,
                strong: 0,
            },
        );
        assert_eq!(dust(&grid, output).power, 0);
        assert_eq!(rs_signal_from(&grid, ORIGIN, RsDir::East, true), 0);

        grid.set_block(source, RsBlock::RedstoneBlock);
        assert_eq!(dust(&grid, output).power, 0);

        grid.set_block(
            source,
            RsBlock::Source {
                weak: 15,
                strong: 11,
            },
        );
        assert_eq!(dust(&grid, output).power, 11);
        assert_eq!(rs_signal_from(&grid, ORIGIN, RsDir::East, true), 11);
        assert_eq!(grid.redstone_block(ORIGIN).direct_signal(RsDir::East), 0);

        let further = RsPos(0, 0, 1);
        grid.set_block(further, RsBlock::Solid);
        assert_eq!(rs_signal_from(&grid, further, RsDir::South, true), 0);

        grid.set_block(ORIGIN, RsBlock::Glass);
        assert_eq!(dust(&grid, output).power, 0);
    }

    #[test]
    fn dust_can_power_a_consumer_through_a_solid_block_without_feeding_dust() {
        let mut grid = RedstoneSimGrid::default();
        let input = RsPos(-1, 0, 0);
        let output = RsPos(1, 0, 0);
        let lamp = RsPos(0, 0, 1);
        grid.set_block(ORIGIN, RsBlock::Solid);
        grid.set_block(input, wire());
        grid.set_block(output, wire());
        grid.set_block(lamp, RsBlock::Lamp { lit: false });
        grid.set_block(RsPos(-2, 0, 0), RsBlock::RedstoneBlock);

        assert_eq!(grid.redstone_block(lamp), RsBlock::Lamp { lit: true });
        assert_eq!(dust(&grid, input).power, 15);
        assert_eq!(dust(&grid, output).power, 0);
    }

    #[test]
    fn wire_steps_both_ways_and_the_lower_cover_controls_the_connection() {
        let mut grid = RedstoneSimGrid::default();
        let upper_support = RsPos(1, 0, 0);
        let upper = upper_support.above();
        let low_source = RsPos(-1, 0, 0);
        let high_source = RsPos(2, 1, 0);
        let east = RsDir::East.horizontal_index().unwrap();
        let west = RsDir::West.horizontal_index().unwrap();

        grid.set_block(ORIGIN, wire());
        grid.set_block(upper_support, RsBlock::Solid);
        grid.set_block(upper, wire());
        grid.set_block(low_source, RsBlock::RedstoneBlock);
        assert_eq!(dust(&grid, ORIGIN).connections[east], RsWireConnection::Up);
        assert_eq!(dust(&grid, upper).connections[west], RsWireConnection::Side);
        assert_eq!(dust(&grid, upper).power, 14);

        grid.set_block(ORIGIN.above(), RsBlock::Solid);
        assert_eq!(dust(&grid, upper).power, 0);
        grid.set_block(ORIGIN.above(), RsBlock::Glass);
        assert_eq!(dust(&grid, upper).power, 14);

        grid.set_block(low_source, RsBlock::Air);
        grid.set_block(high_source, RsBlock::RedstoneBlock);
        assert_eq!(dust(&grid, ORIGIN).power, 14);
        grid.set_block(ORIGIN.above(), RsBlock::Solid);
        assert_eq!(dust(&grid, ORIGIN).power, 0);

        // A glass stair supports the top dust but only the upper wire reads across it.
        grid.set_block(ORIGIN.above(), RsBlock::Glass);
        grid.set_block(upper_support, RsBlock::Glass);
        assert_eq!(dust(&grid, ORIGIN).power, 0);
        grid.set_block(high_source, RsBlock::Air);
        grid.set_block(low_source, RsBlock::RedstoneBlock);
        assert_eq!(dust(&grid, upper).power, 14);
    }

    #[test]
    fn floor_and_wall_torches_invert_the_attachment_after_two_ticks() {
        let mut grid = RedstoneSimGrid::default();
        let floor = ORIGIN.above();
        let wall = ORIGIN.relative(RsDir::East);
        let input = ORIGIN.relative(RsDir::West);
        grid.set_block(ORIGIN, RsBlock::Solid);
        grid.set_block(floor, RsBlock::Torch(RedstoneTorchBlock { lit: true }));
        grid.set_block(
            wall,
            RsBlock::WallTorch(RedstoneWallTorchBlock {
                facing: RsDir::East,
                lit: true,
            }),
        );
        assert_eq!(grid.redstone_block(floor).weak_signal(RsDir::Down), 0);
        assert_eq!(grid.redstone_block(floor).direct_signal(RsDir::Up), 15);
        assert_eq!(grid.redstone_block(wall).weak_signal(RsDir::West), 0);
        assert_eq!(grid.redstone_block(wall).weak_signal(RsDir::East), 15);
        assert_eq!(grid.redstone_block(wall).direct_signal(RsDir::East), 0);

        grid.set_block(
            input,
            RsBlock::Source {
                weak: 15,
                strong: 15,
            },
        );
        for (ticks, expected) in [(1, 15), (1, 0)] {
            grid.advance(ticks);
            assert_eq!(
                grid.redstone_block(floor).weak_signal(RsDir::East),
                expected
            );
            assert_eq!(grid.redstone_block(wall).weak_signal(RsDir::East), expected);
        }
        grid.set_block(input, RsBlock::Air);
        for (ticks, expected) in [(1, 0), (1, 15)] {
            grid.advance(ticks);
            assert_eq!(
                grid.redstone_block(floor).weak_signal(RsDir::East),
                expected
            );
            assert_eq!(grid.redstone_block(wall).weak_signal(RsDir::East), expected);
        }
    }

    #[test]
    fn torch_wire_oscillator_burns_out_on_the_eighth_shutoff() {
        let mut grid = RedstoneSimGrid::default();
        let torch = ORIGIN.above();
        let circuit = [
            RsPos(1, 1, 0),
            RsPos(2, 0, 0),
            RsPos(2, 0, 1),
            RsPos(2, 0, 2),
            RsPos(1, 0, 2),
            RsPos(0, 0, 2),
            RsPos(0, 0, 1),
        ];
        grid.set_block(ORIGIN, RsBlock::Solid);
        for pos in circuit {
            grid.set_block(pos.relative(RsDir::Down), RsBlock::Solid);
            grid.set_block(pos, wire());
        }
        grid.set_block(torch, RsBlock::Torch(RedstoneTorchBlock { lit: true }));

        for shutoff in 1..=8 {
            assert_eq!(
                grid.redstone_block(torch),
                RsBlock::Torch(RedstoneTorchBlock { lit: true })
            );
            grid.advance(2);
            assert_eq!(
                grid.redstone_block(torch),
                RsBlock::Torch(RedstoneTorchBlock { lit: false }),
                "shutoff {shutoff}"
            );
            if shutoff < 8 {
                grid.advance(2);
            }
        }
        grid.advance(159);
        assert_eq!(
            grid.redstone_block(torch),
            RsBlock::Torch(RedstoneTorchBlock { lit: false })
        );
        grid.advance(1);
        assert_eq!(
            grid.redstone_block(torch),
            RsBlock::Torch(RedstoneTorchBlock { lit: true })
        );
    }

    #[test]
    fn torch_burnout_window_includes_tick_sixty_and_expires_afterward() {
        let torch = ORIGIN.above();
        let input = ORIGIN.relative(RsDir::West);
        let on = RsBlock::Source {
            weak: 15,
            strong: 15,
        };
        for (last_interval, relights) in [(12, false), (13, true)] {
            let mut grid = RedstoneSimGrid::default();
            grid.set_block(ORIGIN, RsBlock::Solid);
            grid.set_block(torch, RsBlock::Torch(RedstoneTorchBlock { lit: true }));
            grid.set_block(input, on);
            grid.advance(2);

            for interval in [8, 8, 8, 8, 8, 8, last_interval] {
                grid.set_block(input, RsBlock::Air);
                grid.advance(interval - 2);
                assert_eq!(
                    grid.redstone_block(torch),
                    RsBlock::Torch(RedstoneTorchBlock { lit: true })
                );
                grid.set_block(input, on);
                grid.advance(2);
            }

            grid.set_block(input, RsBlock::Air);
            grid.advance(2);
            assert_eq!(
                grid.redstone_block(torch),
                RsBlock::Torch(RedstoneTorchBlock { lit: relights })
            );
        }
    }

    #[test]
    fn repeater_delay_and_pulse_extension_cover_all_settings() {
        let rear = ORIGIN.relative(RsDir::West);
        let front = ORIGIN.relative(RsDir::East);
        let wire_after_front = front.relative(RsDir::East);
        for (delay, ticks) in [(1, 2), (2, 4), (3, 6), (4, 8)] {
            let mut grid = RedstoneSimGrid::default();
            grid.set_block(ORIGIN, repeater(RsDir::East, delay));
            grid.set_block(front, RsBlock::Solid);
            grid.set_block(wire_after_front, wire());
            grid.set_block(rear, RsBlock::RedstoneBlock);
            grid.advance(1);
            grid.set_block(rear, RsBlock::Air);
            grid.advance(ticks - 2);
            assert_eq!(
                repeater_state(&grid, ORIGIN),
                (false, false),
                "delay {delay}"
            );
            grid.advance(1);
            assert_eq!(
                repeater_state(&grid, ORIGIN),
                (true, false),
                "delay {delay}"
            );
            assert_eq!(grid.redstone_block(ORIGIN).direct_signal(RsDir::West), 0);
            assert_eq!(dust(&grid, wire_after_front).power, 15);

            grid.advance(ticks - 1);
            assert_eq!(
                repeater_state(&grid, ORIGIN),
                (true, false),
                "delay {delay}"
            );
            grid.advance(1);
            assert_eq!(
                repeater_state(&grid, ORIGIN),
                (false, false),
                "delay {delay}"
            );
            assert_eq!(dust(&grid, wire_after_front).power, 0);
        }
    }

    #[test]
    fn perpendicular_diodes_lock_the_repeater_in_either_state() {
        let mut grid = RedstoneSimGrid::default();
        let data = ORIGIN.relative(RsDir::West);
        let lock = ORIGIN.relative(RsDir::North);
        let clock = lock.relative(RsDir::North);

        grid.set_block(ORIGIN, repeater(RsDir::East, 1));
        grid.set_block(lock, RsBlock::RedstoneBlock);
        assert_eq!(repeater_state(&grid, ORIGIN), (false, false));

        grid.set_block(lock, repeater(RsDir::South, 1));
        grid.set_block(clock, RsBlock::RedstoneBlock);
        grid.advance(2);
        assert_eq!(repeater_state(&grid, ORIGIN), (false, true));
        grid.set_block(data, RsBlock::RedstoneBlock);
        grid.advance(4);
        assert_eq!(repeater_state(&grid, ORIGIN), (false, true));

        grid.set_block(clock, RsBlock::Air);
        grid.advance(2);
        assert_eq!(repeater_state(&grid, ORIGIN), (false, false));
        grid.advance(2);
        assert_eq!(repeater_state(&grid, ORIGIN), (true, false));

        let south = ORIGIN.relative(RsDir::South);
        let west_of_south = south.relative(RsDir::West);
        grid.set_block(west_of_south, RsBlock::RedstoneBlock);
        grid.set_block(south, comparator(RsDir::East, RsComparatorMode::Compare, 0));
        grid.advance(2);
        assert_eq!(repeater_state(&grid, ORIGIN), (true, false));

        grid.set_block(west_of_south, RsBlock::Air);
        grid.set_block(south.relative(RsDir::South), RsBlock::Container(3));
        grid.set_block(
            south,
            comparator(RsDir::North, RsComparatorMode::Compare, 0),
        );
        grid.advance(2);
        assert_eq!(repeater_state(&grid, ORIGIN), (true, true));
        grid.set_block(data, RsBlock::Air);
        grid.advance(4);
        assert_eq!(repeater_state(&grid, ORIGIN), (true, true));
        grid.set_block(south, RsBlock::Air);
        grid.advance(2);
        assert_eq!(repeater_state(&grid, ORIGIN), (false, false));
    }

    #[test]
    fn side_lock_wins_over_a_rising_edge_due_on_the_same_tick() {
        let mut grid = RedstoneSimGrid::default();
        let data = ORIGIN.relative(RsDir::West);
        let lock = ORIGIN.relative(RsDir::North);
        let clock = lock.relative(RsDir::North);
        grid.set_block(ORIGIN, repeater(RsDir::East, 1));
        grid.set_block(lock, repeater(RsDir::South, 1));
        grid.set_block(
            data,
            RsBlock::Source {
                weak: 0,
                strong: 15,
            },
        );
        grid.set_block(clock, RsBlock::RedstoneBlock);

        grid.advance(2);
        assert_eq!(repeater_state(&grid, ORIGIN), (false, true));
        grid.set_block(clock, RsBlock::Air);
        grid.advance(2);
        assert_eq!(repeater_state(&grid, ORIGIN), (false, false));
        grid.advance(2);
        assert_eq!(repeater_state(&grid, ORIGIN), (true, false));
    }

    #[test]
    fn comparator_modes_read_wire_sides_and_container_rear_through_one_solid() {
        let mut grid = RedstoneSimGrid::default();
        let rear = ORIGIN.relative(RsDir::West);
        let side = ORIGIN.relative(RsDir::North);
        grid.set_block(
            ORIGIN,
            comparator(RsDir::East, RsComparatorMode::Compare, 0),
        );
        grid.set_block(rear, RsBlock::Solid);
        let container = container_analog_output_signal(&[(37, 64); 27], 27);
        grid.set_block(rear.relative(RsDir::West), RsBlock::Container(container));
        grid.set_block(side, wire());
        grid.set_block(
            side.relative(RsDir::North),
            RsBlock::Source { weak: 6, strong: 0 },
        );

        grid.advance(1);
        assert_eq!(grid.redstone_block(ORIGIN).direct_signal(RsDir::East), 0);
        grid.advance(1);
        assert_eq!(grid.redstone_block(ORIGIN).direct_signal(RsDir::East), 9);
        assert_eq!(grid.redstone_block(ORIGIN).direct_signal(RsDir::West), 0);

        grid.set_block(
            ORIGIN,
            comparator(RsDir::East, RsComparatorMode::Subtract, 9),
        );
        grid.advance(2);
        assert_eq!(grid.redstone_block(ORIGIN).direct_signal(RsDir::East), 3);

        grid.set_block(rear, RsBlock::Glass);
        grid.advance(2);
        assert_eq!(grid.redstone_block(ORIGIN).direct_signal(RsDir::East), 0);

        grid.set_block(rear, RsBlock::Solid);
        grid.set_block(
            rear.relative(RsDir::Down),
            RsBlock::Source {
                weak: 12,
                strong: 12,
            },
        );
        grid.advance(2);
        assert_eq!(grid.redstone_block(ORIGIN).direct_signal(RsDir::East), 3);

        grid.set_block(
            rear.relative(RsDir::Down),
            RsBlock::Source {
                weak: 15,
                strong: 15,
            },
        );
        grid.advance(2);
        assert_eq!(grid.redstone_block(ORIGIN).direct_signal(RsDir::East), 9);

        grid.set_block(rear, RsBlock::Container(6));
        grid.set_block(
            ORIGIN,
            comparator(RsDir::East, RsComparatorMode::Compare, 6),
        );
        grid.advance(2);
        assert_eq!(grid.redstone_block(ORIGIN).direct_signal(RsDir::East), 6);
        grid.set_block(
            ORIGIN,
            comparator(RsDir::East, RsComparatorMode::Subtract, 6),
        );
        grid.advance(2);
        assert_eq!(grid.redstone_block(ORIGIN).direct_signal(RsDir::East), 0);

        grid.set_block(
            ORIGIN,
            comparator(RsDir::East, RsComparatorMode::Compare, 0),
        );
        grid.set_block(side, RsBlock::RedstoneBlock);
        grid.advance(2);
        assert_eq!(grid.redstone_block(ORIGIN).direct_signal(RsDir::East), 0);
    }

    #[test]
    fn container_signal_uses_normalized_slot_fill() {
        assert_eq!(container_analog_output_signal(&[], 0), 0);
        assert_eq!(container_analog_output_signal(&[(0, 64); 27], 27), 0);
        assert_eq!(container_analog_output_signal(&[(1, 64)], 27), 1);
        assert_eq!(container_analog_output_signal(&[(64, 64)], 27), 1);
        assert_eq!(container_analog_output_signal(&[(32, 64); 27], 27), 8);
        assert_eq!(container_analog_output_signal(&[(64, 64); 27], 27), 15);
        assert_eq!(
            container_analog_output_signal(&[(32, 64), (8, 16), (0, 1)], 3),
            5
        );
    }

    #[test]
    fn piston_quasi_connectivity_and_lamp_off_delay() {
        let mut grid = RedstoneSimGrid::default();
        let front = ORIGIN.relative(RsDir::East);
        grid.set_block(front, RsBlock::RedstoneBlock);
        assert!(!is_piston_powered(&grid, ORIGIN, RsDir::East));
        grid.set_block(front, RsBlock::Air);
        let qc = ORIGIN.above().relative(RsDir::West);
        grid.set_block(qc, RsBlock::RedstoneBlock);
        assert_eq!(rs_neighbor_signal(&grid, ORIGIN, true), 0);
        assert!(is_piston_powered(&grid, ORIGIN, RsDir::East));

        grid.set_block(ORIGIN, RsBlock::Lamp { lit: false });
        assert_eq!(grid.redstone_block(ORIGIN), RsBlock::Lamp { lit: false });
        let side = ORIGIN.relative(RsDir::West);
        grid.set_block(side, RsBlock::RedstoneBlock);
        assert_eq!(grid.redstone_block(ORIGIN), RsBlock::Lamp { lit: true });
        grid.set_block(side, RsBlock::Air);
        grid.advance(3);
        assert_eq!(grid.redstone_block(ORIGIN), RsBlock::Lamp { lit: true });
        grid.advance(1);
        assert_eq!(grid.redstone_block(ORIGIN), RsBlock::Lamp { lit: false });

        grid.set_block(side, RsBlock::RedstoneBlock);
        grid.set_block(side, RsBlock::Air);
        grid.advance(3);
        grid.set_block(side, RsBlock::RedstoneBlock);
        grid.advance(1);
        assert_eq!(grid.redstone_block(ORIGIN), RsBlock::Lamp { lit: true });
    }

    #[test]
    fn hopper_inverts_neighbor_power_and_dispensers_trigger_on_edges() {
        let mut grid = RedstoneSimGrid::default();
        let hopper = RsPos(0, 0, 1);
        let dropper = RsPos(2, 0, 0);
        let qc = RsPos(1, 1, 0);
        let hopper_input = RsPos(0, 0, 2);
        grid.set_block(
            ORIGIN,
            RsBlock::Dispenser {
                triggered: false,
                facing: RsDir::North,
            },
        );
        grid.set_block(
            dropper,
            RsBlock::Dropper {
                triggered: false,
                facing: RsDir::North,
            },
        );
        grid.set_block(
            hopper,
            RsBlock::Hopper {
                enabled: true,
                facing: RsDir::Down,
            },
        );
        grid.set_block(qc, RsBlock::RedstoneBlock);
        grid.set_block(hopper_input, RsBlock::RedstoneBlock);
        assert_eq!(
            grid.redstone_block(ORIGIN),
            RsBlock::Dispenser {
                triggered: true,
                facing: RsDir::North
            }
        );
        assert_eq!(
            grid.redstone_block(dropper),
            RsBlock::Dropper {
                triggered: true,
                facing: RsDir::North
            }
        );
        assert_eq!(
            grid.redstone_block(hopper),
            RsBlock::Hopper {
                enabled: false,
                facing: RsDir::Down
            }
        );
        grid.advance(3);
        assert!(grid.take_dispense_events().is_empty());
        grid.advance(1);
        assert_eq!(grid.take_dispense_events(), [ORIGIN, dropper]);
        grid.advance(10);
        assert!(grid.take_dispense_events().is_empty());

        grid.set_block(qc, RsBlock::Air);
        grid.set_block(hopper_input, RsBlock::Air);
        assert_eq!(
            grid.redstone_block(hopper),
            RsBlock::Hopper {
                enabled: true,
                facing: RsDir::Down
            }
        );
        assert_eq!(
            grid.redstone_block(ORIGIN),
            RsBlock::Dispenser {
                triggered: false,
                facing: RsDir::North
            }
        );
        grid.set_block(qc, RsBlock::RedstoneBlock);
        grid.set_block(qc, RsBlock::Air);
        grid.advance(4);
        assert_eq!(grid.take_dispense_events(), [ORIGIN, dropper]);
    }

    #[test]
    fn scheduled_ticks_order_by_due_tick_priority_and_sequence() {
        let mut grid = RedstoneSimGrid::default();
        let later = RsPos(0, 0, 0);
        let second = RsPos(1, 0, 0);
        let first = RsPos(2, 0, 0);
        let third = RsPos(3, 0, 0);
        let removed = RsPos(4, 0, 0);
        for pos in [later, second, first, third, removed] {
            grid.set_block(
                pos,
                RsBlock::Dispenser {
                    triggered: false,
                    facing: RsDir::North,
                },
            );
        }
        grid.schedule(later, 3, RsTickPriority::ExtremelyHigh);
        grid.schedule(second, 2, RsTickPriority::Normal);
        grid.schedule(removed, 1, RsTickPriority::Normal);
        grid.schedule(first, 2, RsTickPriority::High);
        grid.schedule(third, 2, RsTickPriority::Normal);
        grid.set_block(removed, RsBlock::Air);
        grid.set_block(
            removed,
            RsBlock::Dispenser {
                triggered: false,
                facing: RsDir::North,
            },
        );
        grid.advance(3);
        assert_eq!(grid.take_dispense_events(), [first, second, third, later]);
    }

    struct MemoryWorld<'a> {
        reg: &'a BReg,
        states: BTreeMap<RsPos, u32>,
        comparator_outputs: BTreeMap<RsPos, u8>,
        analog: BTreeMap<RsPos, u8>,
        item_frames: BTreeMap<RsPos, (RsDir, u8)>,
        daylight: BTreeMap<RsPos, (u8, i64, bool, bool)>,
        entities: Vec<RsAabb>,
        vibrations: BTreeMap<RsPos, RsSculkVibration>,
        dispensed: Vec<RsPos>,
        updates: VecDeque<RsPos>,
        pending: Vec<(u64, RsWorldTick)>,
        now: u64,
    }

    impl<'a> MemoryWorld<'a> {
        fn new(reg: &'a BReg) -> Self {
            Self {
                reg,
                states: BTreeMap::new(),
                comparator_outputs: BTreeMap::new(),
                analog: BTreeMap::new(),
                item_frames: BTreeMap::new(),
                daylight: BTreeMap::new(),
                entities: Vec::new(),
                vibrations: BTreeMap::new(),
                dispensed: Vec::new(),
                updates: VecDeque::new(),
                pending: Vec::new(),
                now: 0,
            }
        }

        fn drain_updates(&mut self, torches: &mut BTreeMap<RsPos, RsTorchHistory>) {
            while let Some(pos) = self.updates.pop_front() {
                RedstoneWorldAdapter::new(self.reg, self, torches).neighbor_changed(pos);
            }
        }

        fn advance(&mut self, to: u64, torches: &mut BTreeMap<RsPos, RsTorchHistory>) {
            assert!(to >= self.now);
            while let Some((index, &(due, tick))) = self
                .pending
                .iter()
                .enumerate()
                .filter(|&(_, &(due, _))| due <= to)
                .min_by_key(|&(index, &(due, tick))| (due, tick.priority, index))
            {
                self.pending.remove(index);
                self.now = due;
                RedstoneWorldAdapter::new(self.reg, self, torches).run_scheduled_tick(
                    tick.pos,
                    tick.block_id,
                    due,
                );
                self.drain_updates(torches);
            }
            self.now = to;
        }
    }

    impl RedstoneBlockWorld for MemoryWorld<'_> {
        fn state_at(&self, pos: RsPos) -> u32 {
            self.states
                .get(&pos)
                .copied()
                .unwrap_or_else(|| self.reg.state_of("air", None))
        }

        fn is_redstone_conductor(&self, pos: RsPos) -> bool {
            self.reg.is_redstone_conductor(self.state_at(pos))
        }

        fn comparator_output(&self, pos: RsPos) -> u8 {
            self.comparator_outputs.get(&pos).copied().unwrap_or(0)
        }

        fn analog_signal(&self, pos: RsPos) -> Option<u8> {
            self.analog.get(&pos).copied()
        }

        fn item_frame_signal(&self, pos: RsPos, facing: RsDir) -> Option<u8> {
            let &(direction, signal) = self.item_frames.get(&pos)?;
            (direction == facing).then_some(signal)
        }

        fn daylight_env(&self, pos: RsPos) -> Option<(u8, i64, bool, bool)> {
            self.daylight.get(&pos).copied()
        }

        fn entity_intersects(&self, pos: RsPos) -> bool {
            let cell = RsAabb {
                min: [f64::from(pos.0), f64::from(pos.1), f64::from(pos.2)],
                max: [
                    f64::from(pos.0) + 1.0,
                    f64::from(pos.1) + 1.0,
                    f64::from(pos.2) + 1.0,
                ],
            };
            rs_tripwire_entity_intersects(cell, &self.entities)
        }

        fn sculk_vibration(&self, pos: RsPos) -> Option<RsSculkVibration> {
            self.vibrations.get(&pos).cloned()
        }

        fn apply_redstone_change(&mut self, pos: RsPos, state: u32, comparator_output: Option<u8>) {
            self.states.insert(pos, state);
            if let Some(output) = comparator_output {
                self.comparator_outputs.insert(pos, output);
            }
            self.updates.extend(RsDir::ALL.map(|dir| pos.relative(dir)));
        }

        fn schedule_redstone_tick(&mut self, tick: RsWorldTick) {
            if !self
                .pending
                .iter()
                .any(|&(_, queued)| (queued.pos, queued.block_id) == (tick.pos, tick.block_id))
            {
                self.pending.push((self.now + tick.delay, tick));
            }
        }

        fn dispense_redstone(&mut self, pos: RsPos) {
            self.dispensed.push(pos);
        }
    }

    #[test]
    fn lever_wire_repeater_lamp_writes_vanilla_ids_at_the_due_ticks() {
        let reg = breg().unwrap();
        let mut world = MemoryWorld::new(reg);
        let mut torches = BTreeMap::new();
        let [lever, wire, repeater, lamp] = [0, 1, 2, 3].map(|x| RsPos(x, 0, 0));
        let lever_id =
            |powered| registered(reg, "lever", &[("face", "floor"), ("powered", powered)]);
        let wire_id = |power| {
            registered(
                reg,
                "redstone_wire",
                &[
                    ("east", "side"),
                    ("west", "side"),
                    ("north", "none"),
                    ("south", "none"),
                    ("power", power),
                ],
            )
        };
        let repeater_id = |powered| {
            registered(
                reg,
                "repeater",
                &[
                    ("facing", "west"),
                    ("delay", "3"),
                    ("locked", "false"),
                    ("powered", powered),
                ],
            )
        };
        let lamp_id = |lit| registered(reg, "redstone_lamp", &[("lit", lit)]);
        for (pos, initial) in [
            (lever, lever_id("false")),
            (wire, registered(reg, "redstone_wire", &[])),
            (repeater, repeater_id("false")),
            (lamp, lamp_id("false")),
        ] {
            world.states.insert(pos, initial);
            world
                .states
                .insert(pos.relative(RsDir::Down), registered(reg, "stone", &[]));
        }
        RedstoneWorldAdapter::new(reg, &mut world, &mut torches).refresh_wire_shape(wire);
        world.drain_updates(&mut torches);
        assert_eq!(world.state_at(wire), wire_id("0"));
        assert!(world.pending.is_empty());

        world.apply_redstone_change(lever, lever_id("true"), None);
        world.drain_updates(&mut torches);
        assert_eq!(world.state_at(lever), lever_id("true"));
        assert_eq!(world.state_at(wire), wire_id("15"));
        let repeat = RsWorldTick {
            pos: repeater,
            block_id: reg.block_id_of(repeater_id("false")).unwrap() as u32,
            delay: 6,
            priority: RsTickPriority::High,
        };
        assert_eq!(world.pending, [(6, repeat)]);
        world.advance(5, &mut torches);
        assert_eq!(world.state_at(repeater), repeater_id("false"));
        assert_eq!(world.state_at(lamp), lamp_id("false"));
        world.advance(6, &mut torches);
        assert_eq!(world.state_at(repeater), repeater_id("true"));
        assert_eq!(world.state_at(lamp), lamp_id("true"));
        assert!(world.pending.is_empty());

        world.apply_redstone_change(lever, lever_id("false"), None);
        world.drain_updates(&mut torches);
        assert_eq!(world.state_at(wire), wire_id("0"));
        assert_eq!(
            world.pending,
            [(
                12,
                RsWorldTick {
                    priority: RsTickPriority::VeryHigh,
                    ..repeat
                }
            )]
        );
        world.advance(12, &mut torches);
        assert_eq!(world.state_at(repeater), repeater_id("false"));
        assert_eq!(
            world.pending,
            [(
                16,
                RsWorldTick {
                    pos: lamp,
                    block_id: reg.block_id_of(lamp_id("true")).unwrap() as u32,
                    delay: 4,
                    priority: RsTickPriority::Normal,
                }
            )]
        );
        world.advance(15, &mut torches);
        assert_eq!(world.state_at(lamp), lamp_id("true"));
        world.advance(16, &mut torches);
        assert_eq!(world.state_at(lamp), lamp_id("false"));
        assert!(world.pending.is_empty());
    }

    #[test]
    fn comparator_reads_and_updates_the_block_entity_independently_of_powered() {
        let reg = breg().unwrap();
        let pos = RsPos(0, 0, 0);
        let rear = pos.relative(RsDir::West);
        let front = pos.relative(RsDir::East);
        let on = registered(
            reg,
            "comparator",
            &[("facing", "west"), ("powered", "true")],
        );
        let off = registered(
            reg,
            "comparator",
            &[("facing", "west"), ("powered", "false")],
        );
        let mut world = MemoryWorld::new(reg);
        let mut torches = BTreeMap::new();
        world.states.insert(pos, off);
        world
            .states
            .insert(front, registered(reg, "redstone_lamp", &[]));
        world.comparator_outputs.insert(pos, 7);
        world.analog.insert(rear, 7);
        {
            let mut adapter = RedstoneWorldAdapter::new(reg, &mut world, &mut torches);
            assert_eq!(adapter.redstone_block(pos).weak_signal(RsDir::East), 0);
            adapter.neighbor_changed(pos);
        }
        assert_eq!(world.pending.len(), 1);
        assert_eq!(world.pending[0].0, 2);
        world.advance(2, &mut torches);
        assert_eq!(world.state_at(pos), on);
        assert_eq!(world.comparator_output(pos), 7);
        assert_eq!(
            world.state_at(front),
            registered(reg, "redstone_lamp", &[("lit", "true")])
        );

        world.analog.insert(rear, 11);
        RedstoneWorldAdapter::new(reg, &mut world, &mut torches).neighbor_changed(pos);
        world.advance(4, &mut torches);
        assert_eq!(world.state_at(pos), on);
        assert_eq!(world.comparator_output(pos), 11);
    }

    #[test]
    fn stepped_dust_reads_wire_power_and_uses_the_full_attachment_face() {
        let reg = breg().unwrap();
        let wire = RsPos(0, 0, 0);
        let step = wire.relative(RsDir::East);
        let mut world = MemoryWorld::new(reg);
        let mut torches = BTreeMap::new();
        world
            .states
            .insert(wire, registered(reg, "redstone_wire", &[]));
        world.states.insert(step, registered(reg, "stone", &[]));
        world.states.insert(
            step.above(),
            registered(reg, "redstone_wire", &[("power", "12")]),
        );
        let expected = |east, power| {
            registered(
                reg,
                "redstone_wire",
                &[
                    ("east", east),
                    ("west", "side"),
                    ("north", "none"),
                    ("south", "none"),
                    ("power", power),
                ],
            )
        };
        {
            let mut adapter = RedstoneWorldAdapter::new(reg, &mut world, &mut torches);
            adapter.refresh_wire_shape(wire);
            adapter.neighbor_changed(wire);
        }
        assert_eq!(world.state_at(wire), expected("up", "11"));

        world.states.insert(
            step,
            registered(
                reg,
                "oak_trapdoor",
                &[("half", "bottom"), ("open", "false")],
            ),
        );
        {
            let mut adapter = RedstoneWorldAdapter::new(reg, &mut world, &mut torches);
            adapter.refresh_wire_shape(wire);
            adapter.neighbor_changed(wire);
        }
        assert_eq!(world.state_at(wire), expected("side", "0"));
    }

    #[test]
    fn powered_dust_emits_using_live_geometry_before_its_stored_shape_is_refreshed() {
        let reg = breg().unwrap();
        let mut world = MemoryWorld::new(reg);
        let mut torches = BTreeMap::new();
        let dust = registered(
            reg,
            "redstone_wire",
            &[
                ("power", "11"),
                ("north", "side"),
                ("south", "side"),
                ("west", "side"),
                ("east", "side"),
            ],
        );
        let side = ORIGIN.relative(RsDir::North);
        world.states.extend([
            (ORIGIN, dust),
            (side, registered(reg, "stone", &[])),
            (
                ORIGIN.relative(RsDir::East),
                registered(reg, "redstone_wire", &[]),
            ),
        ]);
        let adapter = RedstoneWorldAdapter::new(reg, &mut world, &mut torches);
        assert_eq!(rs_signal_from(&adapter, ORIGIN, RsDir::East, true), 11);
        assert_eq!(rs_signal_from(&adapter, ORIGIN, RsDir::North, true), 0);
        assert_eq!(rs_signal_from(&adapter, side, RsDir::North, true), 0);
        assert_eq!(adapter.world.state_at(ORIGIN), dust);
    }

    #[test]
    fn mounted_controls_observers_and_targets_emit_in_the_vanilla_directions() {
        let reg = breg().unwrap();
        for name in ["lever", "polished_blackstone_button"] {
            for (face, attachment) in [
                ("floor", RsDir::Down),
                ("ceiling", RsDir::Up),
                ("wall", RsDir::West),
            ] {
                let id = registered(
                    reg,
                    name,
                    &[("face", face), ("facing", "east"), ("powered", "true")],
                );
                let block = rs_block_from_state(reg, id);
                for dir in RsDir::ALL {
                    assert_eq!(block.weak_signal(dir), 15, "{name} {face} {dir:?}");
                    assert_eq!(
                        block.direct_signal(dir),
                        if dir == attachment { 15 } else { 0 },
                        "{name} {face} {dir:?}"
                    );
                }
            }
        }

        let observer = registered(reg, "observer", &[("facing", "east"), ("powered", "true")]);
        let target = registered(reg, "target", &[("power", "9")]);
        for dir in RsDir::ALL {
            let observer = rs_block_from_state(reg, observer);
            let expected = if dir == RsDir::West { 15 } else { 0 };
            assert_eq!(observer.weak_signal(dir), expected);
            assert_eq!(observer.direct_signal(dir), expected);
            let target = rs_block_from_state(reg, target);
            assert_eq!(target.weak_signal(dir), 9);
            assert_eq!(target.direct_signal(dir), 0);
        }

        let mut world = MemoryWorld::new(reg);
        let mut torches = BTreeMap::new();
        let wire = RsPos(0, 0, 0);
        world.states.insert(wire.relative(RsDir::East), observer);
        world.states.insert(wire.relative(RsDir::West), observer);
        let adapter = RedstoneWorldAdapter::new(reg, &mut world, &mut torches);
        assert_eq!(
            rs_wire_connection(&adapter, wire, RsDir::East),
            RsWireConnection::Side
        );
        assert_eq!(
            rs_wire_connection(&adapter, wire, RsDir::West),
            RsWireConnection::None
        );
    }

    #[test]
    fn a_pending_tick_does_not_run_on_a_replacement_block() {
        let reg = breg().unwrap();
        let pos = RsPos(0, 0, 0);
        let lamp_off = registered(reg, "redstone_lamp", &[("lit", "false")]);
        let lamp_on = registered(reg, "redstone_lamp", &[("lit", "true")]);
        let torch_off = registered(reg, "redstone_torch", &[("lit", "false")]);
        let mut world = MemoryWorld::new(reg);
        let mut torches = BTreeMap::new();
        world.states.insert(pos, lamp_on);
        RedstoneWorldAdapter::new(reg, &mut world, &mut torches).neighbor_changed(pos);
        assert_eq!(world.pending.len(), 1);
        assert_eq!(
            world.pending[0].1.block_id,
            reg.block_id_of(lamp_off).unwrap() as u32
        );
        world.states.insert(pos, torch_off);
        world.advance(4, &mut torches);
        assert_eq!(world.state_at(pos), torch_off);
    }
    #[test]
    fn registry_batch_settles_dust_and_delivers_queued_ticks_after_power_falls() {
        let reg = breg().unwrap();
        let mut world = MemoryWorld::new(reg);
        let mut torches = BTreeMap::new();
        let wire = registered(reg, "redstone_wire", &[]);
        let dispenser = RsPos(12, 0, 0);
        let initial_dispenser = registered(
            reg,
            "dispenser",
            &[("facing", "south"), ("triggered", "false")],
        );
        world.states.insert(dispenser, initial_dispenser);
        world.states.extend((1..12).map(|x| (RsPos(x, 0, 0), wire)));
        let target = registered(reg, "target", &[("power", "15")]);
        world.states.insert(ORIGIN, target);
        RedstoneWorldAdapter::new(reg, &mut world, &mut torches).propagate(&[ORIGIN]);
        assert_eq!(
            reg.get_prop(world.state_at(RsPos(11, 0, 0)), "power")
                .as_deref(),
            Some("5")
        );
        assert_eq!(
            world.state_at(dispenser),
            reg.with_prop(initial_dispenser, "triggered", "true")
        );

        world.advance(1, &mut torches);
        world.states.remove(&ORIGIN);
        RedstoneWorldAdapter::new(reg, &mut world, &mut torches).propagate(&[ORIGIN]);
        for x in 1..12 {
            assert_eq!(
                reg.get_prop(world.state_at(RsPos(x, 0, 0)), "power")
                    .as_deref(),
                Some("0")
            );
        }
        assert_eq!(world.state_at(dispenser), initial_dispenser);
        world.advance(3, &mut torches);
        assert!(world.dispensed.is_empty());
        world.advance(4, &mut torches);
        assert_eq!(world.dispensed, [dispenser]);
        assert!(world.pending.is_empty());
    }

    #[test]
    fn comparator_uses_the_stronger_analog_source_behind_a_conductor() {
        let reg = breg().unwrap();
        let mut world = MemoryWorld::new(reg);
        let mut torches = BTreeMap::new();
        let rear = ORIGIN.relative(RsDir::West);
        let behind = rear.relative(RsDir::West);
        let comparator = registered(reg, "comparator", &[("facing", "west")]);
        world.states.insert(ORIGIN, comparator);
        world.states.insert(rear, registered(reg, "stone", &[]));
        world.analog.insert(behind, 3);
        world.item_frames.insert(behind, (RsDir::West, 7));
        RedstoneWorldAdapter::new(reg, &mut world, &mut torches).propagate(&[behind]);
        world.advance(2, &mut torches);
        assert_eq!(world.comparator_output(ORIGIN), 7);

        world.item_frames.insert(behind, (RsDir::East, 7));
        RedstoneWorldAdapter::new(reg, &mut world, &mut torches).propagate(&[behind]);
        world.advance(4, &mut torches);
        assert_eq!(world.comparator_output(ORIGIN), 3);
    }

    #[test]
    fn daylight_detector_tracks_solar_cycle_weather_and_inversion() {
        let noon = rs_daylight_detector_signal(0, 6_000, false, false, false);
        let midnight = rs_daylight_detector_signal(0, 18_000, false, false, false);
        assert_eq!(noon, 15);
        assert_eq!(midnight, 0);
        assert_eq!(
            rs_daylight_detector_signal(0, 30_000, false, false, false),
            noon
        );
        assert_eq!(rs_daylight_detector_signal(0, 6_000, false, false, true), 0);
        assert_eq!(
            rs_daylight_detector_signal(0, 18_000, false, false, true),
            15
        );
        let clear = rs_daylight_detector_signal(0, 6_000, false, false, false);
        let rain = rs_daylight_detector_signal(0, 6_000, true, false, false);
        let storm = rs_daylight_detector_signal(0, 6_000, true, true, false);
        assert!(rain < clear);
        assert!(storm < rain);
        assert_eq!(
            rs_daylight_detector_signal(15, 18_000, false, false, false),
            4
        );
        assert_eq!(
            rs_daylight_detector_signal(255, 6_000, false, false, false),
            15
        );
    }

    #[test]
    fn tripwire_requires_opposed_attached_hooks_and_respects_shears() {
        use RsTripwireCell::{Hook, Other, Wire};
        let clear_wire = Wire {
            powered: false,
            disarmed: false,
            entity_intersects: false,
        };
        let connected = rs_tripwire_line(
            RsDir::East,
            &[
                clear_wire,
                Hook {
                    facing: RsDir::West,
                    attached: true,
                },
            ],
        );
        assert_eq!(
            connected,
            RsTripwireResult {
                attached: true,
                powered: false,
                wire_count: 1,
                wires: vec![RsTripwireWireResult {
                    attached: true,
                    powered: false
                }],
            }
        );

        let intersecting_wire = Wire {
            powered: false,
            disarmed: false,
            entity_intersects: true,
        };
        let triggered = rs_tripwire_line(
            RsDir::East,
            &[
                intersecting_wire,
                Hook {
                    facing: RsDir::West,
                    attached: true,
                },
            ],
        );
        assert!(triggered.attached && triggered.powered);
        assert_eq!(
            triggered.wires,
            [RsTripwireWireResult {
                attached: true,
                powered: true
            }]
        );
        let disarmed = rs_tripwire_line(
            RsDir::East,
            &[
                Wire {
                    powered: false,
                    disarmed: true,
                    entity_intersects: true,
                },
                Hook {
                    facing: RsDir::West,
                    attached: true,
                },
            ],
        );
        assert!(disarmed.attached && !disarmed.powered);
        let wire_box = RsAabb {
            min: [0.1, 0.1, 0.1],
            max: [0.9, 0.2, 0.9],
        };
        assert!(rs_tripwire_entity_intersects(
            wire_box,
            &[RsAabb {
                min: [0.5, 0.15, 0.5],
                max: [1.5, 1.5, 1.5],
            }]
        ));
        assert!(!rs_tripwire_entity_intersects(
            wire_box,
            &[RsAabb {
                min: [1.0, 0.0, 0.0],
                max: [2.0, 1.0, 1.0],
            }]
        ));
        for cells in [
            vec![
                Wire {
                    powered: false,
                    disarmed: false,
                    entity_intersects: true,
                },
                Hook {
                    facing: RsDir::East,
                    attached: true,
                },
            ],
            vec![
                Wire {
                    powered: false,
                    disarmed: false,
                    entity_intersects: false,
                },
                Hook {
                    facing: RsDir::West,
                    attached: false,
                },
            ],
            vec![
                Wire {
                    powered: false,
                    disarmed: false,
                    entity_intersects: false,
                },
                Other,
                Hook {
                    facing: RsDir::West,
                    attached: true,
                },
            ],
        ] {
            assert!(!rs_tripwire_line(RsDir::East, &cells).attached);
            assert!(!rs_tripwire_line(RsDir::East, &cells).powered);
        }
        let forty = vec![
            Wire {
                powered: false,
                disarmed: false,
                entity_intersects: false,
            };
            40
        ]
        .into_iter()
        .chain([Hook {
            facing: RsDir::West,
            attached: true,
        }])
        .collect::<Vec<_>>();
        assert!(rs_tripwire_line(RsDir::East, &forty).attached);
        let forty_one = vec![
            Wire {
                powered: false,
                disarmed: false,
                entity_intersects: false,
            };
            41
        ]
        .into_iter()
        .chain([Hook {
            facing: RsDir::West,
            attached: true,
        }])
        .collect::<Vec<_>>();
        assert!(!rs_tripwire_line(RsDir::East, &forty_one).attached);
    }

    #[test]
    fn sculk_sensor_frequency_attenuation_occlusion_and_phases() {
        let frequencies = [
            ("step", 1),
            ("swim", 1),
            ("flap", 1),
            ("projectile_land", 2),
            ("hit_ground", 2),
            ("splash", 3),
            ("fluid_pickup", 3),
            ("fluid_place", 3),
            ("item_interact_start", 4),
            ("projectile_shoot", 4),
            ("block_destroy", 5),
            ("dispense_fail", 6),
            ("block_place", 7),
            ("entity_place", 7),
            ("entity_damage", 8),
            ("eat", 9),
            ("drink", 9),
            ("container_open", 10),
            ("block_activate", 11),
            ("block_attach", 11),
            ("block_detach", 11),
            ("entity_roar", 12),
            ("equip", 12),
            ("entity_die", 13),
            ("teleport", 13),
            ("explode", 14),
            ("lightning_strike", 15),
        ];
        for (event, expected) in frequencies {
            assert_eq!(rs_sculk_frequency(event), Some(expected), "{event}");
        }
        assert_eq!(rs_sculk_frequency("minecraft:step"), Some(1));
        assert_eq!(rs_sculk_frequency("unknown_event"), None);

        let sensor = |distance, occluded, filter, calibrated, elapsed| {
            rs_sculk_sensor_evaluate("step", distance, 8, occluded, filter, calibrated, elapsed)
        };
        assert_eq!(sensor(0.0, false, None, false, Some(0)).power, 15);
        assert_eq!(sensor(4.0, false, None, false, Some(0)).power, 8);
        assert_eq!(sensor(8.0, false, None, false, Some(0)).power, 0);
        assert_eq!(sensor(2.0, true, None, false, Some(0)).power, 0);
        assert_eq!(sensor(2.0, false, Some(2), true, Some(0)).power, 0);
        assert_eq!(sensor(2.0, false, Some(1), true, Some(0)).power, 12);
        assert_eq!(
            sensor(2.0, false, None, false, Some(29)).phase,
            RsSculkPhase::Active
        );
        assert_eq!(
            sensor(2.0, false, None, false, Some(30)).phase,
            RsSculkPhase::Cooldown
        );
        assert_eq!(
            sensor(2.0, false, None, false, Some(39)).phase,
            RsSculkPhase::Cooldown
        );
        assert_eq!(
            sensor(2.0, false, None, false, Some(40)).phase,
            RsSculkPhase::Inactive
        );
        assert_eq!(
            sensor(2.0, false, None, true, Some(9)).phase,
            RsSculkPhase::Active
        );
        assert_eq!(
            sensor(2.0, false, None, true, Some(10)).phase,
            RsSculkPhase::Cooldown
        );
        assert_eq!(
            sensor(2.0, false, None, true, Some(20)).phase,
            RsSculkPhase::Inactive
        );
    }

    #[test]
    fn redstone_world_decodes_sensor_state_properties() {
        let reg = breg().unwrap();
        let daylight = registered(reg, "daylight_detector", &[("inverted", "true")]);
        assert_eq!(
            rs_daylight_detector_state(reg, daylight),
            Some(RsDaylightDetectorState { inverted: true })
        );
        let wire = registered(
            reg,
            "tripwire",
            &[
                ("attached", "true"),
                ("disarmed", "true"),
                ("powered", "false"),
            ],
        );
        assert_eq!(
            rs_tripwire_cell_from_state(reg, wire),
            Some(RsTripwireCell::Wire {
                powered: false,
                disarmed: true,
                entity_intersects: false,
            })
        );
        let hook = registered(
            reg,
            "tripwire_hook",
            &[
                ("attached", "true"),
                ("facing", "east"),
                ("powered", "true"),
            ],
        );
        assert_eq!(
            rs_tripwire_cell_from_state(reg, hook),
            Some(RsTripwireCell::Hook {
                facing: RsDir::East,
                attached: true
            })
        );
        assert_eq!(
            rs_tripwire_hook_state(reg, hook),
            Some(RsTripwireHookState {
                facing: RsDir::East,
                attached: true,
                powered: true
            })
        );
        let sensor = registered(
            reg,
            "sculk_sensor",
            &[("power", "0"), ("sculk_sensor_phase", "inactive")],
        );
        assert_eq!(
            rs_sculk_sensor_state(reg, sensor),
            Some(RsSculkSensorState {
                calibrated: false,
                phase: RsSculkPhase::Inactive,
                power: 0
            })
        );
        let calibrated = registered(
            reg,
            "calibrated_sculk_sensor",
            &[("power", "12"), ("sculk_sensor_phase", "active")],
        );
        assert_eq!(
            rs_sculk_sensor_state(reg, calibrated),
            Some(RsSculkSensorState {
                calibrated: true,
                phase: RsSculkPhase::Active,
                power: 12
            })
        );
    }

    #[test]
    fn redstone_sensors_daylight_tripwire_sculk_live_wire() {
        let reg = breg().unwrap();
        let mut torches = BTreeMap::new();
        let prop = |world: &MemoryWorld, pos: RsPos, key: &str| {
            reg.get_prop(world.state_at(pos), key)
        };

        // --- Daylight detector: stored power is authoritative until the world
        // reports the environment, then the evaluator owns it. Normal mode
        // emits in daylight; inverted mode emits at night.
        let mut world = MemoryWorld::new(reg);
        let detector = RsPos(0, 0, 0);
        let dust = RsPos(0, 0, 1);
        world.states.insert(
            detector,
            registered(reg, "daylight_detector", &[("power", "0")]),
        );
        world
            .states
            .insert(dust, registered(reg, "redstone_wire", &[("power", "0")]));
        RedstoneWorldAdapter::new(reg, &mut world, &mut torches).propagate(&[detector]);
        world.drain_updates(&mut torches);
        assert_eq!(prop(&world, dust, "power").as_deref(), Some("0"));

        world.daylight.insert(detector, (15, 6_000, false, false));
        RedstoneWorldAdapter::new(reg, &mut world, &mut torches).propagate(&[detector]);
        assert_eq!(prop(&world, detector, "power").as_deref(), Some("15"));
        world.drain_updates(&mut torches);
        assert_eq!(prop(&world, dust, "power").as_deref(), Some("15"));

        // Inverted: dark at noon, lit at midnight (11 = 15 - max(0, 15 - 11)).
        world.apply_redstone_change(
            detector,
            registered(
                reg,
                "daylight_detector",
                &[("inverted", "true"), ("power", "0")],
            ),
            None,
        );
        RedstoneWorldAdapter::new(reg, &mut world, &mut torches).propagate(&[detector]);
        world.drain_updates(&mut torches);
        assert_eq!(prop(&world, detector, "power").as_deref(), Some("0"));
        assert_eq!(prop(&world, dust, "power").as_deref(), Some("0"));
        world.daylight.insert(detector, (15, 18_000, false, false));
        RedstoneWorldAdapter::new(reg, &mut world, &mut torches).propagate(&[detector]);
        assert_eq!(prop(&world, detector, "power").as_deref(), Some("11"));
        world.drain_updates(&mut torches);
        assert_eq!(prop(&world, dust, "power").as_deref(), Some("11"));

        // --- Tripwire hook: entity on the line powers the hook, the wire cells
        // and the dust beside the hook; clearing the entity shuts it off.
        let mut world = MemoryWorld::new(reg);
        let hook = RsPos(10, 0, 0);
        let cell = RsPos(11, 0, 0);
        let far_hook = RsPos(12, 0, 0);
        let dust = RsPos(10, 0, 1);
        world.states.extend([
            (
                RsPos(9, 0, 0),
                registered(reg, "stone", &[]),
            ),
            (
                hook,
                registered(
                    reg,
                    "tripwire_hook",
                    &[("attached", "false"), ("facing", "east")],
                ),
            ),
            (cell, registered(reg, "tripwire", &[("disarmed", "false")])),
            (
                far_hook,
                registered(
                    reg,
                    "tripwire_hook",
                    &[("attached", "true"), ("facing", "west")],
                ),
            ),
            (
                RsPos(13, 0, 0),
                registered(reg, "stone", &[]),
            ),
            (dust, registered(reg, "redstone_wire", &[("power", "0")])),
        ]);
        RedstoneWorldAdapter::new(reg, &mut world, &mut torches).propagate(&[hook]);
        world.drain_updates(&mut torches);
        assert_eq!(prop(&world, hook, "powered").as_deref(), Some("false"));
        assert_eq!(prop(&world, dust, "power").as_deref(), Some("0"));

        world.entities.push(RsAabb {
            min: [11.25, 0.0, 0.25],
            max: [11.75, 1.0, 0.75],
        });
        RedstoneWorldAdapter::new(reg, &mut world, &mut torches).propagate(&[hook]);
        assert_eq!(prop(&world, hook, "attached").as_deref(), Some("true"));
        assert_eq!(prop(&world, hook, "powered").as_deref(), Some("true"));
        assert_eq!(prop(&world, cell, "attached").as_deref(), Some("true"));
        assert_eq!(prop(&world, cell, "powered").as_deref(), Some("true"));
        world.drain_updates(&mut torches);
        assert_eq!(prop(&world, dust, "power").as_deref(), Some("15"));

        world.entities.clear();
        RedstoneWorldAdapter::new(reg, &mut world, &mut torches).propagate(&[hook]);
        world.drain_updates(&mut torches);
        assert_eq!(prop(&world, hook, "powered").as_deref(), Some("false"));
        assert_eq!(prop(&world, dust, "power").as_deref(), Some("0"));

        // --- Sculk sensor: a block_place vibration powers the dust for the
        // vanilla 30-tick pulse, then cools down and re-arms.
        let mut world = MemoryWorld::new(reg);
        let sensor = RsPos(20, 0, 0);
        let dust = RsPos(20, 0, 1);
        world.states.insert(sensor, registered(reg, "sculk_sensor", &[]));
        world
            .states
            .insert(dust, registered(reg, "redstone_wire", &[("power", "0")]));
        world.vibrations.insert(
            sensor,
            RsSculkVibration {
                event: "block_place".into(),
                distance: 2.0,
                wool_occluded: false,
            },
        );
        RedstoneWorldAdapter::new(reg, &mut world, &mut torches).propagate(&[sensor]);
        assert_eq!(
            prop(&world, sensor, "sculk_sensor_phase").as_deref(),
            Some("active")
        );
        assert_eq!(prop(&world, sensor, "power").as_deref(), Some("12"));
        world.drain_updates(&mut torches);
        assert_eq!(prop(&world, dust, "power").as_deref(), Some("12"));

        world.advance(30, &mut torches);
        assert_eq!(
            prop(&world, sensor, "sculk_sensor_phase").as_deref(),
            Some("cooldown")
        );
        world.drain_updates(&mut torches);
        assert_eq!(prop(&world, dust, "power").as_deref(), Some("0"));
        world.advance(40, &mut torches);
        assert_eq!(
            prop(&world, sensor, "sculk_sensor_phase").as_deref(),
            Some("inactive")
        );

        // --- Calibrated sensor: 10-tick pulse, frequency filter on the amethyst
        // input side, no output on the side opposite the input.
        let mut world = MemoryWorld::new(reg);
        let sensor = RsPos(30, 0, 0);
        let input = sensor.relative(RsDir::North); // facing side
        let quiet = sensor.relative(RsDir::South); // opposite the input
        let loud = sensor.relative(RsDir::East);
        world.states.extend([
            (
                sensor,
                registered(
                    reg,
                    "calibrated_sculk_sensor",
                    &[("facing", "north"), ("power", "0")],
                ),
            ),
            (
                input,
                registered(
                    reg,
                    "lever",
                    &[("face", "floor"), ("powered", "true")],
                ),
            ),
            (
                quiet,
                registered(reg, "redstone_wire", &[("power", "0")]),
            ),
            (
                loud,
                registered(reg, "redstone_wire", &[("power", "0")]),
            ),
        ]);
        world.vibrations.insert(
            sensor,
            RsSculkVibration {
                event: "lightning_strike".into(), // frequency 15 = filter input
                distance: 2.0,
                wool_occluded: false,
            },
        );
        RedstoneWorldAdapter::new(reg, &mut world, &mut torches).propagate(&[sensor]);
        assert_eq!(
            prop(&world, sensor, "sculk_sensor_phase").as_deref(),
            Some("active")
        );
        assert_eq!(prop(&world, sensor, "power").as_deref(), Some("14"));
        world.drain_updates(&mut torches);
        assert_eq!(prop(&world, loud, "power").as_deref(), Some("14"));
        assert_eq!(prop(&world, quiet, "power").as_deref(), Some("0"));

        // A filtered-out frequency never activates the sensor.
        world.vibrations.insert(
            sensor,
            RsSculkVibration {
                event: "explode".into(), // frequency 14 != filter 15
                distance: 1.0,
                wool_occluded: false,
            },
        );
        world.advance(20, &mut torches);
        assert_eq!(
            prop(&world, sensor, "sculk_sensor_phase").as_deref(),
            Some("inactive")
        );
        world.vibrations.remove(&sensor);
        RedstoneWorldAdapter::new(reg, &mut world, &mut torches).propagate(&[sensor]);
        world.drain_updates(&mut torches);
        assert_eq!(
            prop(&world, sensor, "sculk_sensor_phase").as_deref(),
            Some("inactive")
        );
        assert_eq!(prop(&world, loud, "power").as_deref(), Some("0"));
    }
}
