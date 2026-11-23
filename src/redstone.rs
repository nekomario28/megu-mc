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

    fn axis(self) -> RsAxis {
        match self {
            Self::West | Self::East => RsAxis::X,
            Self::Down | Self::Up => RsAxis::Y,
            Self::North | Self::South => RsAxis::Z,
        }
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

    fn steps(self, dir: RsDir, count: i32) -> Self {
        let (dx, dy, dz) = dir.offset();
        Self(
            self.0 + dx * count,
            self.1 + dy * count,
            self.2 + dz * count,
        )
    }

    fn above(self) -> Self {
        self.relative(RsDir::Up)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RsAxis {
    X,
    Y,
    Z,
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

    /// The analog source a comparator would read at `pos`, for hosts that model
    /// analog blocks beyond [`RsBlock::Container`] (cauldrons, composters,
    /// lecterns, jukeboxes...). Fed through `rs_comparator_signal`, the single
    /// owner of every analog output formula.
    fn analog_source(&self, _pos: RsPos) -> Option<RsAnalogSource> {
        None
    }

    fn container_analog_signal(&self, pos: RsPos) -> Option<u8> {
        if let Some(source) = self.analog_source(pos) {
            return Some(rs_comparator_signal(&source));
        }
        match self.redstone_block(pos) {
            RsBlock::Container(signal) => Some(signal),
            _ => None,
        }
    }

    /// A `moving_piston` block entity still in flight at `pos`, which
    /// `rs_piston_retraction_event` reads for the one-tick-pulse drop rule.
    fn moving_piston_at(&self, _pos: RsPos) -> Option<RsMovingPiston> {
        None
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

    // Vanilla `RedStoneWireBlock.shouldConnectTo`: repeaters connect along
    // their axis and observers connect only on their output (back) face.
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

// ---------- Vanilla 1.21.1 analog comparator outputs ----------

/// A block family's analog output inputs. `getAnalogOutputSignal` lives on the
/// block (or its block entity) in vanilla; these are the exact per-family
/// formulas from the 1.21.1 server classes.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug)]
enum RsAnalogSource {
    /// (item count, per-slot max stack) per occupied container slot; the
    /// vanilla formula divides each by `min(item_max, container_max)`.
    Container { slots: Vec<(u32, u32)>, num_slots: usize },
    /// `LayeredCauldronBlock`: water and powder snow read LEVEL directly.
    LayeredCauldron { level: u8 },
    /// `LavaCauldronBlock` is a constant 3.
    LavaCauldron,
    /// `ComposterBlock` reads LEVEL (0..=8) directly.
    Composter { level: u8 },
    /// `CakeBlock.getOutputSignal`: `(7 - bites) * 2`.
    Cake { bites: u8 },
    /// `LecternBlockEntity.getRedstoneSignal`:
    /// `floor(page/(pageCount-1) * 14) + 1`, or 15 for a one-page book; no
    /// book means no comparator source at all.
    Lectern { has_book: bool, page: u16, page_count: u16 },
    /// `EndPortalFrameBlock`: 15 while HAS_EYE.
    EndPortalFrame { eye: bool },
    /// `RespawnAnchorBlock.getScaledAnalogOutput`:
    /// `floor((charges - min) / (max - min) * 15)`.
    RespawnAnchor { charges: u8 },
    /// `DecoratedPotBlock` defers to the one-slot container formula.
    DecoratedPot { count: u32, max_stack: u32 },
    /// `ChiseledBookShelfBlock`: `lastInteractedSlot + 1`, 0 with no last slot.
    ChiseledBookshelf { last_slot: Option<u8> },
    /// `BeehiveBlock` reads HONEY_LEVEL (0..=5) directly.
    Beehive { honey_level: u8 },
    /// `JukeboxBlockEntity`: the loaded record's jukebox_song
    /// `comparator_output`, or 0 when empty.
    Jukebox { song_output: Option<u8> },
    /// `CommandBlock`: the command chain's success count.
    CommandBlock { success_count: i32 },
}

#[cfg_attr(not(test), allow(dead_code))]
fn rs_comparator_signal(source: &RsAnalogSource) -> u8 {
    match source {
        RsAnalogSource::Container { slots, num_slots } => {
            container_analog_output_signal(slots, *num_slots)
        }
        RsAnalogSource::LayeredCauldron { level } => (*level).min(3),
        RsAnalogSource::LavaCauldron => 3,
        RsAnalogSource::Composter { level } => (*level).min(8),
        RsAnalogSource::Cake { bites } => 7u8.saturating_sub((*bites).min(6)) * 2,
        RsAnalogSource::Lectern {
            has_book,
            page,
            page_count,
        } => {
            if !*has_book {
                return 0;
            }
            let ratio = if *page_count > 1 {
                (*page).min(*page_count - 1) as f32 / (*page_count - 1) as f32
            } else {
                1.0
            };
            ((ratio * 14.0).floor() as u8 + 1).min(15)
        }
        RsAnalogSource::EndPortalFrame { eye } => u8::from(*eye) * 15,
        RsAnalogSource::RespawnAnchor { charges } => {
            ((*charges).min(4) as f32 / 4.0 * 15.0).floor() as u8
        }
        RsAnalogSource::DecoratedPot { count, max_stack } => {
            container_analog_output_signal(&[(*count, *max_stack)], 1)
        }
        RsAnalogSource::ChiseledBookshelf { last_slot } => {
            last_slot.map_or(0, |slot| slot.min(5) + 1)
        }
        RsAnalogSource::Beehive { honey_level } => (*honey_level).min(5),
        RsAnalogSource::Jukebox { song_output } => song_output.unwrap_or(0).min(15),
        RsAnalogSource::CommandBlock { success_count } => {
            (*success_count).clamp(0, 15) as u8
        }
    }
}

/// Vanilla 1.21.1 jukebox_song `comparator_output` values (the record item's
/// registry entry), by disc item name.
#[cfg_attr(not(test), allow(dead_code))]
fn rs_jukebox_song_output(disc: &str) -> Option<u8> {
    Some(match disc {
        "music_disc_13" => 1,
        "music_disc_cat" => 2,
        "music_disc_blocks" => 3,
        "music_disc_chirp" => 4,
        "music_disc_far" => 5,
        "music_disc_mall" => 6,
        "music_disc_mellohi" => 7,
        "music_disc_stal" => 8,
        "music_disc_strad" => 9,
        "music_disc_ward" => 10,
        "music_disc_11" | "music_disc_creator_music_box" => 11,
        "music_disc_wait" | "music_disc_creator" => 12,
        "music_disc_pigstep" | "music_disc_precipice" => 13,
        "music_disc_otherside" | "music_disc_relic" => 14,
        "music_disc_5" => 15,
        _ => return None,
    })
}

// ---------- Vanilla 1.21.1 target block (dog) ----------

const RS_TARGET_PROJECTILE_PULSE_TICKS: u64 = 20;
const RS_TARGET_PULSE_TICKS: u64 = 8;

/// `TargetBlock.getSignal` (dog): the radial hit strength in the plane
/// perpendicular to the hit face — `max(1, ceil(15 * (0.5 - d) / 0.5))` where
/// `d` is the larger of the two perpendicular |frac(coord) - 0.5| offsets.
/// Arrows and other projectiles get the same signal; the projectile flag only
/// sets the longer power-off schedule.
#[cfg_attr(not(test), allow(dead_code))]
fn rs_target_block_signal(hit: [f64; 3], face_axis: RsAxis) -> u8 {
    let frac = |v: f64| (v - v.floor()).abs();
    let dx = (frac(hit[0]) - 0.5).abs();
    let dy = (frac(hit[1]) - 0.5).abs();
    let dz = (frac(hit[2]) - 0.5).abs();
    let d = match face_axis {
        RsAxis::X => dy.max(dz),
        RsAxis::Y => dx.max(dz),
        RsAxis::Z => dx.max(dy),
    };
    let t = ((0.5 - d) / 0.5).clamp(0.0, 1.0);
    ((15.0 * t).ceil() as u8).max(1)
}

/// The power-off delay `TargetBlock` schedules: 20 game ticks for arrows
/// (`instanceof AbstractArrow`), 8 for every other projectile.
#[cfg_attr(not(test), allow(dead_code))]
fn rs_target_pulse_ticks(arrow: bool) -> u64 {
    if arrow {
        RS_TARGET_PROJECTILE_PULSE_TICKS
    } else {
        RS_TARGET_PULSE_TICKS
    }
}

/// Vanilla `TargetBlock.onProjectileHit`: the radial hit strength is stored on
/// the block and the power-off tick is queued through the same scheduler every
/// other device uses. `rs_run_tick` clears the stored power when it fires.
#[cfg_attr(not(test), allow(dead_code))]
fn rs_target_projectile_hit(
    world: &mut impl RedstoneUpdates,
    pos: RsPos,
    hit: [f64; 3],
    face_axis: RsAxis,
    arrow: bool,
) {
    if !matches!(world.redstone_block(pos), RsBlock::Target { .. }) {
        return;
    }
    let power = rs_target_block_signal(hit, face_axis);
    world.set_component(pos, RsBlock::Target { power });
    world.schedule(
        pos,
        rs_target_pulse_ticks(arrow),
        RsTickPriority::Normal,
    );
}

// ---------- Vanilla 1.21.1 piston push model (dsv/dsz) ----------

/// `PistonBaseBlock#MAX_PUSH` / `PistonStructureResolver#a`.
const RS_PISTON_MAX_PUSH: usize = 12;

/// `PushReaction` (epj) as it affects `isPushable`.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum RsPushReaction {
    #[default]
    Normal,
    /// Breaks instead of moving when *pushed* (torches, flowers, crops, snow).
    Destroy,
    /// Never movable (moving_piston, spawners via BLOCK, ...).
    Block,
    /// Only pushable in the piston's facing direction.
    PushOnly,
}

/// Slime/honey glue families (`isStickyBlock`).
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum RsSticky {
    #[default]
    None,
    Slime,
    Honey,
}

impl RsSticky {
    fn sticks(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Everything `isPushable` (dsv.a) reads off a block state.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default)]
struct RsPushBlock {
    air: bool,
    /// The hard-coded list: obsidian, crying_obsidian, respawn_anchor,
    /// reinforced_deepslate.
    immovable: bool,
    /// `destroySpeed == -1`: bedrock, barrier, end_portal_frame, command
    /// blocks, spawners, structure blocks, portals/gateways.
    unbreakable: bool,
    /// A piston or sticky_piston body (retracted bases are pushable, extended
    /// ones are not and skip the breakability/reaction checks).
    piston_base: bool,
    extended: bool,
    /// `hasBlockEntity`: chests, furnaces, signs and every other BE block.
    block_entity: bool,
    reaction: RsPushReaction,
    sticky: RsSticky,
}

/// `PistonBaseBlock.isPushable` minus the world-border checks, which the
/// resolver applies separately via `rs_piston_is_pushable_at`.
#[cfg_attr(not(test), allow(dead_code))]
fn rs_piston_is_pushable(
    block: &RsPushBlock,
    allow_destroy: bool,
    same_direction: bool,
) -> bool {
    if block.air {
        return true;
    }
    if block.immovable {
        return false;
    }
    if block.piston_base {
        if block.extended {
            return false;
        }
    } else {
        if block.unbreakable {
            return false;
        }
        match block.reaction {
            RsPushReaction::Block => return false,
            RsPushReaction::Destroy => return allow_destroy,
            RsPushReaction::PushOnly => return same_direction,
            RsPushReaction::Normal => {}
        }
    }
    !block.block_entity
}

/// `PistonStructureResolver.canStickToEachOther` (dsz.a(dtc,dtc)): slime and
/// honey never stick to each other; otherwise the pair sticks when either side
/// is sticky — a sticky block also drags plain pushable neighbours.
#[cfg_attr(not(test), allow(dead_code))]
fn rs_piston_can_stick(front: RsSticky, back: RsSticky) -> bool {
    match (front, back) {
        (RsSticky::Slime, RsSticky::Honey) | (RsSticky::Honey, RsSticky::Slime) => false,
        _ => front.sticks() || back.sticks(),
    }
}

/// The block-storage half of `PistonStructureResolver`: a position lookup plus
/// the vanilla `isPushable` world bounds (build height, world border).
#[cfg_attr(not(test), allow(dead_code))]
trait RsPistonWorld {
    fn piston_block(&self, pos: RsPos) -> RsPushBlock;
    fn in_world_border(&self, _pos: RsPos) -> bool {
        true
    }
    /// (min_y, max_y) build range, like `level.getMinBuildHeight() /
    /// getMaxBuildHeight()`.
    fn build_height(&self) -> (i32, i32) {
        (i32::MIN / 2, i32::MAX / 2)
    }
}

/// The world-border half of `PistonBaseBlock.isPushable`: out-of-bounds and
/// build-edge moves are refused before the block itself is examined.
#[cfg_attr(not(test), allow(dead_code))]
fn rs_piston_is_pushable_at(
    world: &impl RsPistonWorld,
    pos: RsPos,
    move_dir: RsDir,
    allow_destroy: bool,
    piston_facing: RsDir,
) -> bool {
    let (min_y, max_y) = world.build_height();
    if pos.1 < min_y || pos.1 > max_y - 1 || !world.in_world_border(pos) {
        return false;
    }
    let block = world.piston_block(pos);
    if block.air {
        return true;
    }
    if block.immovable {
        return false;
    }
    if move_dir == RsDir::Down && pos.1 == min_y {
        return false;
    }
    if move_dir == RsDir::Up && pos.1 == max_y - 1 {
        return false;
    }
    rs_piston_is_pushable(&block, allow_destroy, move_dir == piston_facing)
}

/// The resolver outcome: every cell the piston actually moves plus the cells
/// whose `DESTROY` reaction ends a push line.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RsPistonPlan {
    to_move: Vec<RsPos>,
    to_destroy: Vec<RsPos>,
}

/// Faithful port of `PistonStructureResolver` (dsz): extension walks forward
/// from the piston face, retraction starts two cells out and pulls. `None`
/// means vanilla `resolve()` returned false — nothing moves.
#[cfg_attr(not(test), allow(dead_code))]
fn rs_piston_resolve(
    world: &impl RsPistonWorld,
    piston_pos: RsPos,
    piston_facing: RsDir,
    extending: bool,
) -> Option<RsPistonPlan> {
    let (push_dir, start) = if extending {
        (piston_facing, piston_pos.relative(piston_facing))
    } else {
        (piston_facing.opposite(), piston_pos.steps(piston_facing, 2))
    };
    let mut resolver = RsPistonResolver {
        world,
        piston_pos,
        push_dir,
        to_move: Vec::new(),
        to_destroy: Vec::new(),
    };
    let head = world.piston_block(start);
    if !rs_piston_is_pushable_at(world, start, push_dir, false, piston_facing) {
        return if extending && head.reaction == RsPushReaction::Destroy {
            Some(RsPistonPlan {
                to_move: Vec::new(),
                to_destroy: vec![start],
            })
        } else {
            None
        };
    }
    if !resolver.add_block_line(start, push_dir) {
        return None;
    }
    let mut index = 0;
    while index < resolver.to_move.len() {
        let pos = resolver.to_move[index];
        let block = world.piston_block(pos);
        if block.sticky.sticks() && !resolver.add_branching_blocks(pos) {
            return None;
        }
        index += 1;
    }
    Some(RsPistonPlan {
        to_move: resolver.to_move,
        to_destroy: resolver.to_destroy,
    })
}

#[cfg_attr(not(test), allow(dead_code))]
struct RsPistonResolver<'w, W: RsPistonWorld> {
    world: &'w W,
    piston_pos: RsPos,
    push_dir: RsDir,
    to_move: Vec<RsPos>,
    to_destroy: Vec<RsPos>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl<W: RsPistonWorld> RsPistonResolver<'_, W> {
    /// `addBlockLine`: walk the line from `pos` along `push_dir`, pulling a
    /// sticky chain behind the anchor first, then walking forward until air,
    /// a DESTROY block, an immovable block, or the 12-block cap.
    fn add_block_line(&mut self, pos: RsPos, dir: RsDir) -> bool {
        let mut state = self.world.piston_block(pos);
        if state.air
            || !rs_piston_is_pushable_at(self.world, pos, self.push_dir, false, dir)
            || pos == self.piston_pos
            || self.to_move.contains(&pos)
        {
            return true;
        }

        let mut back_count = 1usize;
        if back_count + self.to_move.len() > RS_PISTON_MAX_PUSH {
            return false;
        }
        while state.sticky.sticks() {
            let back = pos.steps(self.push_dir.opposite(), back_count as i32);
            let prev = state;
            state = self.world.piston_block(back);
            if state.air
                || !rs_piston_can_stick(prev.sticky, state.sticky)
                || !rs_piston_is_pushable_at(
                    self.world,
                    back,
                    self.push_dir,
                    false,
                    self.push_dir.opposite(),
                )
                || back == self.piston_pos
            {
                break;
            }
            back_count += 1;
            if back_count + self.to_move.len() > RS_PISTON_MAX_PUSH {
                return false;
            }
        }

        let mut added = 0usize;
        for step in (0..back_count).rev() {
            self.to_move
                .push(pos.steps(self.push_dir.opposite(), step as i32));
            added += 1;
        }

        let mut ahead = 1i32;
        loop {
            let front = pos.steps(self.push_dir, ahead);
            if let Some(collision) = self.to_move.iter().position(|&p| p == front) {
                reorder_list_at_collision(&mut self.to_move, added, collision);
                for index in 0..=collision + added {
                    let cell = self.to_move[index];
                    let block = self.world.piston_block(cell);
                    if block.sticky.sticks() && !self.add_branching_blocks(cell) {
                        return false;
                    }
                }
                return true;
            }
            state = self.world.piston_block(front);
            if state.air {
                return true;
            }
            if !rs_piston_is_pushable_at(self.world, front, self.push_dir, true, self.push_dir)
                || front == self.piston_pos
            {
                return false;
            }
            if state.reaction == RsPushReaction::Destroy {
                self.to_destroy.push(front);
                return true;
            }
            if self.to_move.len() >= RS_PISTON_MAX_PUSH {
                return false;
            }
            self.to_move.push(front);
            added += 1;
            ahead += 1;
        }
    }

    /// `addBranchingBlocks`: sticky cells pull perpendicular neighbours that
    /// `canStickToEachOther` matches; each pulled neighbour resolves its own
    /// line along that branch direction.
    fn add_branching_blocks(&mut self, pos: RsPos) -> bool {
        let state = self.world.piston_block(pos);
        let push_axis = self.push_dir.axis();
        for dir in RsDir::ALL {
            if dir.axis() == push_axis {
                continue;
            }
            let neighbour = pos.relative(dir);
            let neighbour_state = self.world.piston_block(neighbour);
            if rs_piston_can_stick(neighbour_state.sticky, state.sticky)
                && !self.add_block_line(neighbour, dir)
            {
                return false;
            }
        }
        true
    }
}

/// `reorderListAtCollision` (dsz.a(int,int)): the newly walked segment is
/// spliced in right after the cell it collided with.
#[cfg_attr(not(test), allow(dead_code))]
fn reorder_list_at_collision(to_move: &mut Vec<RsPos>, added: usize, collision: usize) {
    let tail_start = to_move.len() - added;
    let mut reordered = Vec::with_capacity(to_move.len());
    reordered.extend_from_slice(&to_move[..collision]);
    reordered.extend_from_slice(&to_move[tail_start..]);
    reordered.extend_from_slice(&to_move[collision..tail_start]);
    *to_move = reordered;
}

/// A `moving_piston` block entity still extending when its base retracts.
/// `progress` is `getProgress(0)` in [0, 1); `last_ticked` is the block
/// entity's last ticked game time.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug)]
struct RsMovingPiston {
    progress: f32,
    last_ticked: u64,
}

/// Vanilla `PistonBaseBlock.checkIfExtend` retraction events: a sticky piston
/// that is switched off while the block it pushed is still a mid-extension
/// `moving_piston` (`progress < 0.5`, same tick, or during tick processing)
/// fires block-event `2` — it "drops" the pushed block instead of pulling it.
/// Event `1` is the normal pull-back. This is the 1-tick pulse rule.
#[cfg_attr(not(test), allow(dead_code))]
fn rs_piston_retraction_event(
    sticky: bool,
    moving_extension: Option<RsMovingPiston>,
    now: u64,
    handling_tick: bool,
) -> u8 {
    const EVENT_PULL: u8 = 1;
    const EVENT_DROP: u8 = 2;
    if !sticky {
        return EVENT_PULL;
    }
    match moving_extension {
        Some(moving)
            if moving.progress < 0.5 || moving.last_ticked == now || handling_tick =>
        {
            EVENT_DROP
        }
        _ => EVENT_PULL,
    }
}

/// The pushability half of `redstone_block`: how `PistonBaseBlock.isPushable`
/// would read each block the engine models. Block entities never move,
/// attached components break, unknown registry states keep vanilla's safest
/// answer — do not move — and the rest are plain movable.
#[cfg_attr(not(test), allow(dead_code))]
fn rs_push_block_of(block: RsBlock) -> RsPushBlock {
    match block {
        RsBlock::Air => RsPushBlock {
            air: true,
            ..Default::default()
        },
        RsBlock::Piston { extended, .. } => RsPushBlock {
            piston_base: true,
            extended,
            ..Default::default()
        },
        RsBlock::Wire(_)
        | RsBlock::Torch(_)
        | RsBlock::WallTorch(_)
        | RsBlock::Lever { .. }
        | RsBlock::Button { .. }
        | RsBlock::TripwireHook { .. } => RsPushBlock {
            reaction: RsPushReaction::Destroy,
            ..Default::default()
        },
        RsBlock::Container(_)
        | RsBlock::Comparator(_)
        | RsBlock::Repeater(_)
        | RsBlock::Hopper { .. }
        | RsBlock::Dispenser { .. }
        | RsBlock::Dropper { .. }
        | RsBlock::DaylightDetector { .. }
        | RsBlock::SculkSensor { .. } => RsPushBlock {
            block_entity: true,
            ..Default::default()
        },
        RsBlock::RegistryState(_) => RsPushBlock {
            reaction: RsPushReaction::Block,
            ..Default::default()
        },
        _ => RsPushBlock::default(),
    }
}

/// Every `RedstoneWorld` is also a `PistonStructureResolver` world: pushability
/// reads straight off `redstone_block`, and vanilla's Overworld build bounds
/// (`minY = -64`, `height = 384`) apply by default.
#[cfg_attr(not(test), allow(dead_code))]
impl<W: RedstoneWorld> RsPistonWorld for W {
    fn piston_block(&self, pos: RsPos) -> RsPushBlock {
        rs_push_block_of(self.redstone_block(pos))
    }

    fn build_height(&self) -> (i32, i32) {
        (-64, 320)
    }
}

/// Vanilla `PistonBaseBlock.triggerEvent`: resolve the structure through
/// [`rs_piston_resolve`], then let the host move the cells. A sticky
/// retraction first asks `rs_piston_retraction_event` whether the mid-flight
/// block must be dropped instead of pulled. Returns false when the resolve
/// failed or the host cannot rewrite blocks — the `extended` flag only flips
/// when the move actually applied.
#[cfg_attr(not(test), allow(dead_code))]
fn rs_piston_apply(
    world: &mut impl RedstoneUpdates,
    pos: RsPos,
    sticky: bool,
    facing: RsDir,
    extending: bool,
    now: u64,
) -> bool {
    let drop_pulled = !extending
        && rs_piston_retraction_event(
            sticky,
            world.moving_piston_at(pos.steps(facing, 2)),
            now,
            false,
        ) == 2;
    // A non-sticky retraction never pulls: vanilla skips the resolver and just
    // retracts the head.
    let plan = if extending || sticky {
        let Some(plan) = rs_piston_resolve(world, pos, facing, extending) else {
            return false;
        };
        plan
    } else {
        RsPistonPlan::default()
    };
    world.apply_piston_plan(pos, facing, extending, &plan, drop_pulled)
}

// ---------- Vanilla 1.21.1 observer (dlj) ----------

/// `ObserverBlock`: the 2-game-tick delay between scheduling and each state
/// flip, which is also the whole powered pulse width.
const RS_OBSERVER_PULSE_TICKS: u64 = 2;

/// Vanilla `ObserverBlock.updateShape`: a change at the observed face
/// schedules the next tick, but only while unpowered — a lit observer ignores
/// further updates until its pulse ends.
#[cfg_attr(not(test), allow(dead_code))]
fn rs_observer_neighbor_update(world: &mut impl RedstoneUpdates, pos: RsPos, changed: RsPos) {
    let RsBlock::Observer { facing, powered } = world.redstone_block(pos) else {
        return;
    };
    if !powered && pos.relative(facing) == changed {
        world.schedule(pos, RS_OBSERVER_PULSE_TICKS, RsTickPriority::Normal);
    }
}

/// Vanilla's `updateShape` fan-out from the *changed* cell: only the observer
/// whose facing points at `changed` may fire, which is exactly what
/// `rs_observer_neighbor_update` checks per candidate position. Hosts call
/// this wherever a block is replaced; the sim's own writers call it too, so
/// observer pulses ride the same entry path as every other notification.
#[cfg_attr(not(test), allow(dead_code))]
fn rs_notify_block_update(world: &mut impl RedstoneUpdates, changed: RsPos) {
    for dir in RsDir::ALL {
        rs_observer_neighbor_update(world, changed.relative(dir), changed);
    }
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
    /// Move the cells `rs_piston_resolve` decided on: `to_destroy` cells break,
    /// `to_move` cells shift one step in the push direction, and the piston
    /// head cell transitions. `drop_pulled` is the one-tick-pulse case where
    /// the pushed block stays where it landed. Hosts that cannot rewrite
    /// blocks report `false`, which keeps the piston's `extended` flag off —
    /// a move must never half-apply.
    fn apply_piston_plan(
        &mut self,
        _pos: RsPos,
        _facing: RsDir,
        _extending: bool,
        _plan: &RsPistonPlan,
        _drop_pulled: bool,
    ) -> bool {
        false
    }

    /// Vanilla `blockEvent` fires once per `neighborChanged` pass: a piston
    /// event whose resolve could not move is consumed and only re-arms on the
    /// next update, never in a tight same-tick loop. Hosts that simulate the
    /// device event queue track the consumed positions for the current tick.
    fn piston_event_consumed(&self, _pos: RsPos) -> bool {
        false
    }

    /// Record that `pos`'s piston event ran and failed to apply; suppresses
    /// re-scheduling until the host's next update pass clears the mark.
    fn consume_piston_event(&mut self, _pos: RsPos) {}
}

#[cfg_attr(not(test), allow(dead_code))]
type RsScheduledTick = Reverse<(u64, RsTickPriority, u64, RsPos)>;

/// `RedstoneLampBlock.neighborChanged` — a lamp stays lit this long after its
/// power cut before `tick` reads `powered` again.
const RS_LAMP_OFF_TICKS: u64 = 4;
/// `DispenserBlock.neighborChanged` / `DropperBlock.neighborChanged`.
const RS_DISPENSER_TICKS: u64 = 4;
/// `SculkSensorBlockEntity` — the cooldown phase length and the active phase
/// lengths (`calibrated` variants listen half as long).
const RS_SCULK_COOLDOWN_TICKS: u64 = 10;
const RS_SCULK_ACTIVE_TICKS: u64 = 30;
const RS_SCULK_CALIBRATED_ACTIVE_TICKS: u64 = 10;
/// `PistonBaseBlock.checkIfExtend` — the block event resolves inside the same
/// neighbor update, so the engine queues a same-tick piston tick.
const RS_PISTON_EVENT_TICKS: u64 = 0;

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
    /// Pistons whose `triggerEvent` resolve failed in the current game tick —
    /// the vanilla "event consumed" rule that stops a powered-but-stuck
    /// piston from re-arming (and re-failing) inside the same tick.
    consumed_piston_events: BTreeSet<RsPos>,
    // Host-surface state the live world serves through `RedstoneBlockWorld`:
    // the sim exposes the same hooks so the canonical engine can exercise the
    // analog, sensor, tripwire and piston paths end to end.
    analog_sources: BTreeMap<RsPos, RsAnalogSource>,
    daylight_envs: BTreeMap<RsPos, (u8, i64, bool, bool)>,
    tripwire_lines: BTreeMap<RsPos, RsTripwireResult>,
    tripwire_cells: BTreeMap<RsPos, RsTripwireWireResult>,
    vibrations: BTreeMap<RsPos, RsSculkVibration>,
    moving_pistons: BTreeMap<RsPos, RsMovingPiston>,
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

    fn analog_source(&self, pos: RsPos) -> Option<RsAnalogSource> {
        self.analog_sources.get(&pos).cloned()
    }

    fn moving_piston_at(&self, pos: RsPos) -> Option<RsMovingPiston> {
        self.moving_pistons.get(&pos).copied()
    }

    fn daylight_env(&self, pos: RsPos) -> Option<(u8, i64, bool, bool)> {
        self.daylight_envs.get(&pos).copied()
    }

    fn tripwire_line(&self, pos: RsPos, _facing: RsDir) -> Option<RsTripwireResult> {
        self.tripwire_lines.get(&pos).cloned()
    }

    fn sculk_vibration(&self, pos: RsPos) -> Option<RsSculkVibration> {
        self.vibrations.get(&pos).cloned()
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl RedstoneSimGrid {
    fn set_block(&mut self, pos: RsPos, block: RsBlock) {
        // A new placement is a fresh `neighborChanged` pass: consumed piston
        // events re-arm, exactly like vanilla re-checking `checkIfExtend`.
        self.consumed_piston_events.clear();
        if std::mem::discriminant(&self.redstone_block(pos)) != std::mem::discriminant(&block) {
            self.pending.remove(&pos);
            self.torches.remove(&pos);
        }
        if block == RsBlock::Air {
            self.blocks.remove(&pos);
        } else {
            self.blocks.insert(pos, block);
        }
        rs_notify_block_update(self, pos);
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
            if due > self.now {
                self.consumed_piston_events.clear();
            }
            self.now = due;
            self.tick_moving_pistons();
            rs_run_tick(self, pos, due);
            self.settle();
        }
        if target > self.now {
            self.consumed_piston_events.clear();
        }
        self.now = target;
        self.tick_moving_pistons();
    }

    /// `PistonMovingBlockEntity.tick`: `progress += 0.5` per game tick and the
    /// moved block lands as a real block at 1.0 — keeping `last_ticked` and
    /// `progress` honest for the sticky retraction drop check.
    fn tick_moving_pistons(&mut self) {
        let now = self.now;
        self.moving_pistons.retain(|_, moving| {
            if moving.last_ticked < now {
                moving.progress += 0.5 * (now - moving.last_ticked) as f32;
                moving.last_ticked = now;
            }
            moving.progress < 1.0
        });
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
        // An engine write is a block update too: observers facing the cell
        // pulse even when the change came from another device's tick.
        rs_notify_block_update(self, pos);
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

    fn update_tripwire_cell(&mut self, pos: RsPos, wire: RsTripwireWireResult) {
        self.tripwire_cells.insert(pos, wire);
    }

    fn piston_event_consumed(&self, pos: RsPos) -> bool {
        self.consumed_piston_events.contains(&pos)
    }

    fn consume_piston_event(&mut self, pos: RsPos) {
        self.consumed_piston_events.insert(pos);
    }

    fn apply_piston_plan(
        &mut self,
        pos: RsPos,
        facing: RsDir,
        extending: bool,
        plan: &RsPistonPlan,
        drop_pulled: bool,
    ) -> bool {
        let push_dir = if extending { facing } else { facing.opposite() };
        // Read the pre-move world first so a contiguous line cannot clobber
        // itself: every target is collected before any origin clears.
        let moved: Vec<(RsPos, RsBlock)> = if drop_pulled {
            Vec::new()
        } else {
            plan.to_move
                .iter()
                .map(|&cell| (cell.steps(push_dir, 1), self.redstone_block(cell)))
                .collect()
        };
        // The head cell vacates either way: extension leaves `moving_piston`
        // there, retraction removes it. The sim has no head state, so the cell
        // reads as air — a real host writes `piston_head` here.
        self.blocks.remove(&pos.relative(facing));
        for &cell in &plan.to_destroy {
            self.blocks.remove(&cell);
        }
        if drop_pulled {
            // Event 2 leaves the pushed block where it landed: the cells stay
            // put and the mid-flight marker is consumed, so a short-pulsed
            // sticky piston can never delete the block it pushed.
            for &cell in &plan.to_move {
                self.moving_pistons.remove(&cell);
            }
        } else {
            for &cell in &plan.to_move {
                self.blocks.remove(&cell);
            }
            for (dst, block) in moved {
                if block == RsBlock::Air {
                    self.blocks.remove(&dst);
                } else {
                    self.blocks.insert(dst, block);
                }
                if extending {
                    // Vanilla moves every pushed cell as a `moving_piston`
                    // block entity: `progress` 0.0 and `last_ticked` now —
                    // the exact state the sticky-drop check reads on a
                    // same-tick or next-tick retraction.
                    self.moving_pistons.insert(
                        dst,
                        RsMovingPiston {
                            progress: 0.0,
                            last_ticked: self.now,
                        },
                    );
                }
            }
        }
        true
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
                world.schedule(pos, RS_LAMP_OFF_TICKS, RsTickPriority::Normal);
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
                world.schedule(pos, RS_DISPENSER_TICKS, RsTickPriority::Normal);
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
                    if calibrated {
                        RS_SCULK_CALIBRATED_ACTIVE_TICKS
                    } else {
                        RS_SCULK_ACTIVE_TICKS
                    },
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
        RsBlock::Piston {
            facing, extended, ..
        } => {
            // Vanilla `PistonBaseBlock.checkIfExtend` — the QC-aware signal read
            // decides whether the block event runs; `rs_run_tick` performs the
            // resolve + apply when it fires. An event this tick already
            // consumed (failed resolve or apply) does not re-arm until the
            // next update pass, matching vanilla's once-per-neighborChanged
            // blockEvent.
            if is_piston_powered(world, pos, facing) != extended
                && !world.piston_event_consumed(pos)
            {
                world.schedule(pos, RS_PISTON_EVENT_TICKS, RsTickPriority::Normal);
            }
            block
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
            world.schedule(pos, RS_SCULK_COOLDOWN_TICKS, RsTickPriority::Normal);
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
        // Vanilla `TargetBlock.tick`: the pulse scheduled at hit time clears
        // the stored power — `rs_target_projectile_hit` wrote both.
        RsBlock::Target { .. } => RsBlock::Target { power: 0 },
        // Vanilla `PistonBaseBlock.triggerEvent`: `checkIfExtend` queued this
        // same-tick event; re-derive power the way the vanilla trigger does,
        // then resolve + apply the move so `extended` only flips when the
        // structure actually moved.
        RsBlock::Piston {
            facing,
            sticky,
            extended,
        } => {
            if is_piston_powered(world, pos, facing) != extended {
                if rs_piston_apply(world, pos, sticky, facing, !extended, now) {
                    RsBlock::Piston {
                        facing,
                        sticky,
                        extended: !extended,
                    }
                } else {
                    // The event ran and could not move the structure (a line
                    // over the 12-block cap, an immovable block, or a host
                    // that cannot rewrite cells): mark it consumed so
                    // `rs_update_device` cannot re-arm it inside this tick.
                    world.consume_piston_event(pos);
                    block
                }
            } else {
                block
            }
        }
        // Vanilla ObserverBlock.tick: a powered observer turns off; an
        // unpowered one turns on and schedules the off-edge 2 ticks out, so
        // each observed change produces a 2-game-tick high pulse.
        RsBlock::Observer { facing, powered } => {
            if powered {
                RsBlock::Observer {
                    facing,
                    powered: false,
                }
            } else {
                world.schedule(pos, RS_OBSERVER_PULSE_TICKS, RsTickPriority::Normal);
                RsBlock::Observer {
                    facing,
                    powered: true,
                }
            }
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
    #[cfg(megu_vanilla_data)]
    use crate::json::jparse;

    const ORIGIN: RsPos = RsPos(0, 0, 0);

    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    fn registered(reg: &BReg, name: &str, properties: &[(&str, &str)]) -> u32 {
        let properties = properties
            .iter()
            .map(|&(key, value)| (key.into(), value.into()))
            .collect::<Vec<_>>();
        reg.checked_state(name, &properties)
            .unwrap_or_else(|| panic!("missing 1.21.1 state {name} {properties:?}"))
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn registry_codec_round_trips_every_supported_1211_state() {
        let _guard = crate::world_guard();
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
    #[cfg(megu_vanilla_data)]
    fn registry_conductor_and_support_use_native_state_flags() {
        let _guard = crate::world_guard();
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
    #[cfg(megu_vanilla_data)]
    fn registry_states_drive_torch_wire_repeater_lamp_circuit() {
        let _guard = crate::world_guard();
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

    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
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
        #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
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

        #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
        fn drain_updates(&mut self, torches: &mut BTreeMap<RsPos, RsTorchHistory>) {
            while let Some(pos) = self.updates.pop_front() {
                RedstoneWorldAdapter::new(self.reg, self, torches).neighbor_changed(pos);
            }
        }

        #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
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
    #[cfg(megu_vanilla_data)]
    fn lever_wire_repeater_lamp_writes_vanilla_ids_at_the_due_ticks() {
        let _guard = crate::world_guard();
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
    #[cfg(megu_vanilla_data)]
    fn comparator_reads_and_updates_the_block_entity_independently_of_powered() {
        let _guard = crate::world_guard();
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
    #[cfg(megu_vanilla_data)]
    fn stepped_dust_reads_wire_power_and_uses_the_full_attachment_face() {
        let _guard = crate::world_guard();
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
    #[cfg(megu_vanilla_data)]
    fn powered_dust_emits_using_live_geometry_before_its_stored_shape_is_refreshed() {
        let _guard = crate::world_guard();
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
    #[cfg(megu_vanilla_data)]
    fn mounted_controls_observers_and_targets_emit_in_the_vanilla_directions() {
        let _guard = crate::world_guard();
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
            RsWireConnection::Side
        );
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn a_pending_tick_does_not_run_on_a_replacement_block() {
        let _guard = crate::world_guard();
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
    #[cfg(megu_vanilla_data)]
    fn registry_batch_settles_dust_and_delivers_queued_ticks_after_power_falls() {
        let _guard = crate::world_guard();
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
    #[cfg(megu_vanilla_data)]
    fn comparator_uses_the_stronger_analog_source_behind_a_conductor() {
        let _guard = crate::world_guard();
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
    #[cfg(megu_vanilla_data)]
    fn redstone_world_decodes_sensor_state_properties() {
        let _guard = crate::world_guard();
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
    #[cfg(megu_vanilla_data)]
    fn redstone_sensors_daylight_tripwire_sculk_live_wire() {
        let _guard = crate::world_guard();
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

#[cfg(test)]
mod p4b_parity_tests {
    use super::*;

    fn push_cell(
        cells: &mut std::collections::BTreeMap<RsPos, RsPushBlock>,
        x: i32,
        y: i32,
        z: i32,
        block: RsPushBlock,
    ) {
        cells.insert(RsPos(x, y, z), block);
    }

    fn movable() -> RsPushBlock {
        RsPushBlock::default()
    }

    fn slime() -> RsPushBlock {
        RsPushBlock {
            sticky: RsSticky::Slime,
            ..Default::default()
        }
    }

    struct LineWorld {
        cells: std::collections::BTreeMap<RsPos, RsPushBlock>,
    }

    impl RsPistonWorld for LineWorld {
        fn piston_block(&self, pos: RsPos) -> RsPushBlock {
            self.cells.get(&pos).copied().unwrap_or(RsPushBlock {
                air: true,
                ..Default::default()
            })
        }
    }

    #[test]
    fn p4b_parity_comparator_analog_containers_empty_one_full() {
        // Inventory fill: empty=0, any non-empty floor(14*fill)+1, full=15.
        let empty = RsAnalogSource::Container {
            slots: vec![],
            num_slots: 27,
        };
        assert_eq!(rs_comparator_signal(&empty), 0);
        // A single item in a 27-slot chest still emits 1, not 0.
        let one = RsAnalogSource::Container {
            slots: vec![(1, 64)],
            num_slots: 27,
        };
        assert_eq!(rs_comparator_signal(&one), 1);
        // One full stack in 27 slots: floor(14/27)+1 = 1.
        let stack = RsAnalogSource::Container {
            slots: vec![(64, 64)],
            num_slots: 27,
        };
        assert_eq!(rs_comparator_signal(&stack), 1);
        // Half of 27 stacks: floor(14*0.5)+1 = 8.
        let half = RsAnalogSource::Container {
            slots: vec![(64, 64); 14].into_iter().take(13).chain([(32, 64)]).collect(),
            num_slots: 27,
        };
        assert_eq!(rs_comparator_signal(&half), 8);
        // Full 27 stacks of 64 -> 15.
        let full = RsAnalogSource::Container {
            slots: vec![(64, 64); 27],
            num_slots: 27,
        };
        assert_eq!(rs_comparator_signal(&full), 15);
        // Furnace-style 3 slots: one item -> floor(14*(1/64)/3)+1 = 1.
        let furnace = RsAnalogSource::Container {
            slots: vec![(1, 64)],
            num_slots: 3,
        };
        assert_eq!(rs_comparator_signal(&furnace), 1);
    }

    #[test]
    fn p4b_parity_comparator_analog_all_block_families() {
        // Layered cauldrons read LEVEL directly; lava is a constant 3.
        for level in 0..=3 {
            assert_eq!(
                rs_comparator_signal(&RsAnalogSource::LayeredCauldron { level }),
                level
            );
        }
        assert_eq!(rs_comparator_signal(&RsAnalogSource::LavaCauldron), 3);
        // Composter: raw 0..=8 level.
        for level in 0..=8 {
            assert_eq!(
                rs_comparator_signal(&RsAnalogSource::Composter { level }),
                level
            );
        }
        // Cake: (7 - bites) * 2 -> 14 down to 2.
        for bites in 0..=6 {
            assert_eq!(
                rs_comparator_signal(&RsAnalogSource::Cake { bites }),
                (7 - bites) * 2
            );
        }
        // Lectern: floor(page/(count-1)*14)+1, 15 for a 1-page book, 0 empty.
        assert_eq!(
            rs_comparator_signal(&RsAnalogSource::Lectern {
                has_book: true,
                page: 0,
                page_count: 1
            }),
            15
        );
        assert_eq!(
            rs_comparator_signal(&RsAnalogSource::Lectern {
                has_book: true,
                page: 0,
                page_count: 10
            }),
            1
        );
        assert_eq!(
            rs_comparator_signal(&RsAnalogSource::Lectern {
                has_book: true,
                page: 9,
                page_count: 10
            }),
            15
        );
        assert_eq!(
            rs_comparator_signal(&RsAnalogSource::Lectern {
                has_book: true,
                page: 4,
                page_count: 9
            }),
            8
        );
        assert_eq!(
            rs_comparator_signal(&RsAnalogSource::Lectern {
                has_book: false,
                page: 0,
                page_count: 0
            }),
            0
        );
        // End portal frame: 15 only with the eye inserted.
        assert_eq!(
            rs_comparator_signal(&RsAnalogSource::EndPortalFrame { eye: true }),
            15
        );
        assert_eq!(
            rs_comparator_signal(&RsAnalogSource::EndPortalFrame { eye: false }),
            0
        );
        // Respawn anchor: floor(charges/4 * 15) -> 0, 3, 7, 11, 15.
        for (charges, signal) in [(0u8, 0u8), (1, 3), (2, 7), (3, 11), (4, 15)] {
            assert_eq!(
                rs_comparator_signal(&RsAnalogSource::RespawnAnchor { charges }),
                signal,
                "charges {charges}"
            );
        }
        // Decorated pot: single-slot container formula.
        assert_eq!(
            rs_comparator_signal(&RsAnalogSource::DecoratedPot {
                count: 0,
                max_stack: 64
            }),
            0
        );
        assert_eq!(
            rs_comparator_signal(&RsAnalogSource::DecoratedPot {
                count: 1,
                max_stack: 64
            }),
            1
        );
        assert_eq!(
            rs_comparator_signal(&RsAnalogSource::DecoratedPot {
                count: 64,
                max_stack: 64
            }),
            15
        );
        // Chiseled bookshelf: last interacted slot + 1, 0 when none.
        assert_eq!(
            rs_comparator_signal(&RsAnalogSource::ChiseledBookshelf { last_slot: None }),
            0
        );
        for slot in 0..6u8 {
            assert_eq!(
                rs_comparator_signal(&RsAnalogSource::ChiseledBookshelf {
                    last_slot: Some(slot)
                }),
                slot + 1
            );
        }
        // Beehive/bee nest reads honey level 0..=5 directly.
        for level in 0..=5 {
            assert_eq!(
                rs_comparator_signal(&RsAnalogSource::Beehive { honey_level: level }),
                level
            );
        }
        // Jukebox: the song's comparator_output; verify the 1.21.1 table and
        // that vanilla values (5 -> 15, pigstep -> 13) match the jar data.
        assert_eq!(rs_jukebox_song_output("music_disc_13"), Some(1));
        assert_eq!(rs_jukebox_song_output("music_disc_cat"), Some(2));
        assert_eq!(rs_jukebox_song_output("music_disc_11"), Some(11));
        assert_eq!(rs_jukebox_song_output("music_disc_creator_music_box"), Some(11));
        assert_eq!(rs_jukebox_song_output("music_disc_wait"), Some(12));
        assert_eq!(rs_jukebox_song_output("music_disc_creator"), Some(12));
        assert_eq!(rs_jukebox_song_output("music_disc_pigstep"), Some(13));
        assert_eq!(rs_jukebox_song_output("music_disc_precipice"), Some(13));
        assert_eq!(rs_jukebox_song_output("music_disc_otherside"), Some(14));
        assert_eq!(rs_jukebox_song_output("music_disc_relic"), Some(14));
        assert_eq!(rs_jukebox_song_output("music_disc_5"), Some(15));
        assert_eq!(rs_jukebox_song_output("music_disc_rellic"), None);
        assert_eq!(
            rs_comparator_signal(&RsAnalogSource::Jukebox {
                song_output: rs_jukebox_song_output("music_disc_ward")
            }),
            10
        );
        assert_eq!(
            rs_comparator_signal(&RsAnalogSource::Jukebox { song_output: None }),
            0
        );
        // Command block passes the success counter through.
        assert_eq!(
            rs_comparator_signal(&RsAnalogSource::CommandBlock { success_count: 7 }),
            7
        );
        assert_eq!(
            rs_comparator_signal(&RsAnalogSource::CommandBlock { success_count: -1 }),
            0
        );
    }

    #[test]
    fn p4b_parity_target_block_bullseye_vs_edge() {
        // Bullseye on a horizontal face: x/z fractions both 0.5 -> 15.
        assert_eq!(rs_target_block_signal([0.5, 0.0, 0.5], RsAxis::Y), 15);
        // Dead-center hits on every face axis -> 15.
        for axis in [RsAxis::X, RsAxis::Y, RsAxis::Z] {
            assert_eq!(rs_target_block_signal([0.5, 0.5, 0.5], axis), 15);
        }
        // Grazing the block edge: perpendicular distance ~0.5 -> signal 1.
        assert_eq!(rs_target_block_signal([0.99, 0.0, 0.5], RsAxis::Y), 1);
        assert_eq!(rs_target_block_signal([0.5, 0.0, 0.99], RsAxis::Y), 1);
        // Halfway between center and edge: |0.75 - 0.5| = 0.25
        // -> ceil(15 * (0.5 - 0.25) / 0.5) = ceil(7.5) = 8.
        assert_eq!(rs_target_block_signal([0.75, 0.0, 0.5], RsAxis::Y), 8);
        // The ignored axis does not count: on an X face only y/z matter.
        assert_eq!(rs_target_block_signal([0.0, 0.5, 0.5], RsAxis::X), 15);
        assert_eq!(rs_target_block_signal([0.0, 0.0, 0.5], RsAxis::X), 1);
        // The larger of the two perpendicular offsets dominates.
        assert_eq!(rs_target_block_signal([0.0, 0.99, 0.5], RsAxis::X), 1);
        assert_eq!(rs_target_block_signal([0.0, 0.75, 0.5], RsAxis::X), 8);
        // Negative coordinates fraction-wrap like Mth.frac.
        assert_eq!(rs_target_block_signal([-8.5, 0.0, -3.5], RsAxis::Y), 15);
        // Pulse lengths: 20 game ticks for arrows, 8 otherwise.
        assert_eq!(rs_target_pulse_ticks(true), 20);
        assert_eq!(rs_target_pulse_ticks(false), 8);
    }

    #[test]
    fn p4b_parity_piston_is_pushable_truth_table() {
        let air = RsPushBlock {
            air: true,
            ..Default::default()
        };
        assert!(rs_piston_is_pushable(&air, false, false));
        // Vanilla hard-coded immovable: obsidian, crying obsidian, respawn
        // anchor, reinforced deepslate.
        let obsidian = RsPushBlock {
            immovable: true,
            ..Default::default()
        };
        assert!(!rs_piston_is_pushable(&obsidian, true, true));
        // destroySpeed -1: bedrock, barrier, end_portal_frame, spawner,
        // command blocks, structure blocks.
        let bedrock = RsPushBlock {
            unbreakable: true,
            ..Default::default()
        };
        assert!(!rs_piston_is_pushable(&bedrock, true, true));
        // Extended pistons never move; retracted ones are pushable.
        let extended = RsPushBlock {
            piston_base: true,
            extended: true,
            ..Default::default()
        };
        assert!(!rs_piston_is_pushable(&extended, true, true));
        let retracted = RsPushBlock {
            piston_base: true,
            ..Default::default()
        };
        assert!(rs_piston_is_pushable(&retracted, true, true));
        // PushReaction::BLOCK is immovable.
        let blocked = RsPushBlock {
            reaction: RsPushReaction::Block,
            ..Default::default()
        };
        assert!(!rs_piston_is_pushable(&blocked, true, true));
        // DESTROY only resolves when the resolver allows breaking (the
        // forward walk passes allow_destroy=true).
        let torch = RsPushBlock {
            reaction: RsPushReaction::Destroy,
            ..Default::default()
        };
        assert!(rs_piston_is_pushable(&torch, true, true));
        assert!(!rs_piston_is_pushable(&torch, false, true));
        // PUSH_ONLY only moves in the piston's facing direction.
        let glazed = RsPushBlock {
            reaction: RsPushReaction::PushOnly,
            ..Default::default()
        };
        assert!(rs_piston_is_pushable(&glazed, false, true));
        assert!(!rs_piston_is_pushable(&glazed, false, false));
        // Block entities never push.
        let chest = RsPushBlock {
            block_entity: true,
            ..Default::default()
        };
        assert!(!rs_piston_is_pushable(&chest, true, true));
        // Slime/honey mutual exclusion, both stick to plain blocks.
        assert!(rs_piston_can_stick(RsSticky::Slime, RsSticky::None));
        assert!(rs_piston_can_stick(RsSticky::Honey, RsSticky::None));
        assert!(rs_piston_can_stick(RsSticky::Slime, RsSticky::Slime));
        assert!(rs_piston_can_stick(RsSticky::Honey, RsSticky::Honey));
        assert!(!rs_piston_can_stick(RsSticky::Slime, RsSticky::Honey));
        assert!(!rs_piston_can_stick(RsSticky::Honey, RsSticky::Slime));
        assert!(!rs_piston_can_stick(RsSticky::None, RsSticky::None));
    }

    #[test]
    fn p4b_parity_piston_twelve_block_push_cap() {
        // 12 movable cells ahead of the piston resolve fine.
        let mut cells = std::collections::BTreeMap::new();
        for i in 1..=12 {
            push_cell(&mut cells, i, 0, 0, movable());
        }
        let world = LineWorld { cells };
        let plan = rs_piston_resolve(&world, RsPos(0, 0, 0), RsDir::East, true)
            .expect("12 blocks push");
        assert_eq!(plan.to_move.len(), 12);
        assert_eq!(plan.to_move[0], RsPos(1, 0, 0));
        assert_eq!(plan.to_move[11], RsPos(12, 0, 0));

        // The 13th cell breaks the resolve.
        let mut cells = std::collections::BTreeMap::new();
        for i in 1..=13 {
            push_cell(&mut cells, i, 0, 0, movable());
        }
        let world = LineWorld { cells };
        assert_eq!(
            rs_piston_resolve(&world, RsPos(0, 0, 0), RsDir::East, true),
            None,
            "13 blocks must fail"
        );

        // An immovable cell anywhere in the line blocks everything.
        let mut cells = std::collections::BTreeMap::new();
        for i in 1..=5 {
            push_cell(&mut cells, i, 0, 0, movable());
        }
        push_cell(
            &mut cells,
            3,
            0,
            0,
            RsPushBlock {
                immovable: true,
                ..Default::default()
            },
        );
        let world = LineWorld { cells };
        assert_eq!(
            rs_piston_resolve(&world, RsPos(0, 0, 0), RsDir::East, true),
            None
        );

        // Obsidian directly at the face: nothing moves.
        let mut cells = std::collections::BTreeMap::new();
        push_cell(
            &mut cells,
            1,
            0,
            0,
            RsPushBlock {
                immovable: true,
                ..Default::default()
            },
        );
        let world = LineWorld { cells };
        assert_eq!(
            rs_piston_resolve(&world, RsPos(0, 0, 0), RsDir::East, true),
            None
        );

        // Bedrock via destroySpeed -1 likewise.
        let mut cells = std::collections::BTreeMap::new();
        push_cell(
            &mut cells,
            1,
            0,
            0,
            RsPushBlock {
                unbreakable: true,
                ..Default::default()
            },
        );
        let world = LineWorld { cells };
        assert_eq!(
            rs_piston_resolve(&world, RsPos(0, 0, 0), RsDir::East, true),
            None
        );

        // A DESTROY-reaction block at the face is destroyed in place with no
        // moves; deeper in the line it ends the push after the movable run.
        let mut cells = std::collections::BTreeMap::new();
        push_cell(
            &mut cells,
            1,
            0,
            0,
            RsPushBlock {
                reaction: RsPushReaction::Destroy,
                ..Default::default()
            },
        );
        let world = LineWorld { cells };
        let plan = rs_piston_resolve(&world, RsPos(0, 0, 0), RsDir::East, true).unwrap();
        assert!(plan.to_move.is_empty());
        assert_eq!(plan.to_destroy, vec![RsPos(1, 0, 0)]);

        let mut cells = std::collections::BTreeMap::new();
        for i in 1..=3 {
            push_cell(&mut cells, i, 0, 0, movable());
        }
        push_cell(
            &mut cells,
            4,
            0,
            0,
            RsPushBlock {
                reaction: RsPushReaction::Destroy,
                ..Default::default()
            },
        );
        push_cell(&mut cells, 5, 0, 0, movable());
        let world = LineWorld { cells };
        let plan = rs_piston_resolve(&world, RsPos(0, 0, 0), RsDir::East, true).unwrap();
        assert_eq!(plan.to_move.len(), 3);
        assert_eq!(plan.to_destroy, vec![RsPos(4, 0, 0)]);
    }

    #[test]
    fn p4b_parity_piston_slime_honey_non_stickiness() {
        // A slime block beside the line is dragged along: pushed line at
        // (1..3, 0, 0) with slime at (1, 1, 0) branches into the move.
        let mut cells = std::collections::BTreeMap::new();
        for i in 1..=2 {
            push_cell(&mut cells, i, 0, 0, slime());
        }
        push_cell(&mut cells, 1, 1, 0, movable());
        let world = LineWorld { cells };
        let plan = rs_piston_resolve(&world, RsPos(0, 0, 0), RsDir::East, true).unwrap();
        assert!(
            plan.to_move.contains(&RsPos(1, 1, 0)),
            "slime drags its plain neighbour"
        );

        // A honey block beside a slime is NOT dragged (mutual exclusion).
        let mut cells = std::collections::BTreeMap::new();
        push_cell(&mut cells, 1, 0, 0, slime());
        push_cell(
            &mut cells,
            1,
            1,
            0,
            RsPushBlock {
                sticky: RsSticky::Honey,
                ..Default::default()
            },
        );
        let world = LineWorld { cells };
        let plan = rs_piston_resolve(&world, RsPos(0, 0, 0), RsDir::East, true).unwrap();
        assert!(
            !plan.to_move.contains(&RsPos(1, 1, 0)),
            "slime cannot pull honey"
        );

        // Symmetric: honey does not pull slime either.
        let mut cells = std::collections::BTreeMap::new();
        push_cell(
            &mut cells,
            1,
            0,
            0,
            RsPushBlock {
                sticky: RsSticky::Honey,
                ..Default::default()
            },
        );
        push_cell(&mut cells, 1, 1, 0, slime());
        let world = LineWorld { cells };
        let plan = rs_piston_resolve(&world, RsPos(0, 0, 0), RsDir::East, true).unwrap();
        assert!(
            !plan.to_move.contains(&RsPos(1, 1, 0)),
            "honey cannot pull slime"
        );
    }

    #[test]
    fn p4b_parity_sticky_piston_one_tick_drop() {
        // Sticky piston pulsed off while its pushed block is still mid-
        // extension (progress < 0.5): block-event 2 -> drop, no pull.
        let mid = RsMovingPiston {
            progress: 0.4,
            last_ticked: 100,
        };
        assert_eq!(rs_piston_retraction_event(true, Some(mid), 110, false), 2);
        // Same tick as the extension started, or inside tick processing.
        let same_tick = RsMovingPiston {
            progress: 0.9,
            last_ticked: 110,
        };
        assert_eq!(rs_piston_retraction_event(true, Some(same_tick), 110, false), 2);
        let old = RsMovingPiston {
            progress: 0.9,
            last_ticked: 100,
        };
        assert_eq!(rs_piston_retraction_event(true, Some(old), 110, true), 2);
        // Extension finished before the pulse ended -> normal pull-back.
        assert_eq!(rs_piston_retraction_event(true, Some(old), 110, false), 1);
        // Nothing mid-extension (head already converted) -> pull.
        assert_eq!(rs_piston_retraction_event(true, None, 110, false), 1);
        // Non-sticky pistons never drop: always the plain event.
        assert_eq!(rs_piston_retraction_event(false, Some(mid), 110, false), 1);
    }

    #[test]
    fn p4b_parity_piston_refuses_moves_outside_build_bounds() {
        // `isPushable`'s world-edge rule: a block cannot be pushed below
        // min_y or above max_y - 1, so the resolver fails at the boundary.
        struct Bounded {
            cells: std::collections::BTreeMap<RsPos, RsPushBlock>,
        }
        impl RsPistonWorld for Bounded {
            fn piston_block(&self, pos: RsPos) -> RsPushBlock {
                self.cells.get(&pos).copied().unwrap_or(RsPushBlock {
                    air: true,
                    ..Default::default()
                })
            }
            fn build_height(&self) -> (i32, i32) {
                (0, 4)
            }
        }
        // Block at min_y pushed down: resolve fails.
        let mut cells = std::collections::BTreeMap::new();
        push_cell(&mut cells, 0, 0, 0, movable());
        let world = Bounded { cells };
        assert_eq!(
            rs_piston_resolve(&world, RsPos(0, 1, 0), RsDir::Down, true),
            None,
            "cannot push a block below min_y"
        );
        // Block at max_y - 1 pushed up: resolve fails.
        let mut cells = std::collections::BTreeMap::new();
        push_cell(&mut cells, 0, 3, 0, movable());
        let world = Bounded { cells };
        assert_eq!(
            rs_piston_resolve(&world, RsPos(0, 2, 0), RsDir::Up, true),
            None,
            "cannot push a block past max_y - 1"
        );
        // The same block moving down off the ceiling is fine.
        assert!(rs_piston_resolve(&world, RsPos(0, 4, 0), RsDir::Down, true).is_some());
        // A cell already outside the world is not pushable at all.
        let mut cells = std::collections::BTreeMap::new();
        push_cell(&mut cells, 0, 4, 0, movable());
        let world = Bounded { cells };
        assert_eq!(
            rs_piston_resolve(&world, RsPos(0, 3, 0), RsDir::Up, true),
            None,
            "a block above the build height never moves"
        );
    }

    #[test]
    fn p4b_parity_observer_two_tick_pulse() {
        let mut grid = RedstoneSimGrid::default();
        let obs = RsPos(0, 0, 0);
        grid.set_block(
            obs,
            RsBlock::Observer {
                facing: RsDir::East,
                powered: false,
            },
        );
        // A change at the watched face schedules the pulse.
        rs_observer_neighbor_update(&mut grid, obs, RsPos(1, 0, 0));
        grid.advance(1);
        assert_eq!(
            grid.redstone_block(obs),
            RsBlock::Observer {
                facing: RsDir::East,
                powered: false
            },
            "no power before the 2-tick tick runs"
        );
        grid.advance(1);
        assert_eq!(
            grid.redstone_block(obs),
            RsBlock::Observer {
                facing: RsDir::East,
                powered: true
            },
            "t+2: pulse high"
        );
        grid.advance(1);
        assert_eq!(
            grid.redstone_block(obs),
            RsBlock::Observer {
                facing: RsDir::East,
                powered: true
            },
            "still high one tick in"
        );
        grid.advance(1);
        assert_eq!(
            grid.redstone_block(obs),
            RsBlock::Observer {
                facing: RsDir::East,
                powered: false
            },
            "t+4: exactly 2 game ticks of power"
        );

        // A change NOT at the observed face does not trigger.
        let mut grid = RedstoneSimGrid::default();
        grid.set_block(
            obs,
            RsBlock::Observer {
                facing: RsDir::East,
                powered: false,
            },
        );
        rs_observer_neighbor_update(&mut grid, obs, RsPos(-1, 0, 0));
        rs_observer_neighbor_update(&mut grid, obs, RsPos(0, 1, 0));
        grid.advance(4);
        assert_eq!(
            grid.redstone_block(obs),
            RsBlock::Observer {
                facing: RsDir::East,
                powered: false
            },
            "side/back changes never power the observer"
        );

        // The pulse emits from the back (opposite the watched face).
        assert_eq!(
            RsBlock::Observer {
                facing: RsDir::East,
                powered: true
            }
            .weak_signal(RsDir::West),
            15
        );
        assert_eq!(
            RsBlock::Observer {
                facing: RsDir::East,
                powered: true
            }
            .weak_signal(RsDir::East),
            0
        );
    }
}

#[cfg(test)]
mod p6c_redstone_parity_tests {
    use super::*;

    #[test]
    fn p6c_parity_comparator_reads_analog_source_through_canonical_engine() {
        // A host-provided analog source flows through
        // `container_analog_signal` -> `rs_comparator_signal` -> `evaluate`,
        // so a jukebox-like read lands on the canonical comparator tick.
        let mut grid = RedstoneSimGrid::default();
        let comp = RsPos(0, 0, 0);
        let rear = comp.relative(RsDir::West);
        grid.analog_sources.insert(
            rear,
            RsAnalogSource::Jukebox {
                song_output: Some(rs_jukebox_song_output("music_disc_11").unwrap()),
            },
        );
        grid.set_block(
            comp,
            RsBlock::Comparator(ComparatorBlock {
                facing: RsDir::East,
                mode: RsComparatorMode::Compare,
                powered: false,
                output: 0,
            }),
        );
        grid.advance(3);
        let RsBlock::Comparator(c) = grid.redstone_block(comp) else {
            panic!("comparator gone");
        };
        assert_eq!(c.output, 11);
        assert!(c.powered);
    }

    #[test]
    fn p6c_parity_observer_pulses_when_sim_block_changes() {
        // `set_block` -> `rs_notify_block_update` ->
        // `rs_observer_neighbor_update` -> `rs_run_tick`: the observer rides
        // the normal entry path instead of a direct helper call.
        let mut grid = RedstoneSimGrid::default();
        let obs = RsPos(0, 0, 0);
        grid.set_block(
            obs,
            RsBlock::Observer {
                facing: RsDir::East,
                powered: false,
            },
        );
        grid.set_block(RsPos(1, 0, 0), RsBlock::RedstoneBlock);
        grid.advance(1);
        assert_eq!(
            grid.redstone_block(obs),
            RsBlock::Observer {
                facing: RsDir::East,
                powered: false
            },
            "the pulse arrives two game ticks after the change"
        );
        grid.advance(1);
        assert_eq!(
            grid.redstone_block(obs),
            RsBlock::Observer {
                facing: RsDir::East,
                powered: true
            }
        );
        grid.advance(2);
        assert_eq!(
            grid.redstone_block(obs),
            RsBlock::Observer {
                facing: RsDir::East,
                powered: false
            },
            "two-game-tick pulse, like vanilla"
        );
    }

    #[test]
    fn p6c_parity_target_hit_stores_power_and_ticks_off() {
        // `rs_target_projectile_hit` writes the radial signal and queues the
        // off tick; `rs_run_tick` clears it — 8 ticks non-arrow, 20 for arrows.
        let mut grid = RedstoneSimGrid::default();
        let target = RsPos(0, 0, 0);
        grid.set_block(target, RsBlock::Target { power: 0 });
        rs_target_projectile_hit(&mut grid, target, [0.5, 0.0, 0.5], RsAxis::Y, false);
        assert_eq!(grid.redstone_block(target), RsBlock::Target { power: 15 });
        grid.advance(8);
        assert_eq!(grid.redstone_block(target), RsBlock::Target { power: 0 });

        rs_target_projectile_hit(&mut grid, target, [0.5, 0.5, 0.0], RsAxis::Z, true);
        assert_eq!(grid.redstone_block(target), RsBlock::Target { power: 15 });
        grid.advance(8);
        assert_eq!(
            grid.redstone_block(target),
            RsBlock::Target { power: 15 },
            "arrows hold the 20-game-tick pulse"
        );
        grid.advance(12);
        assert_eq!(grid.redstone_block(target), RsBlock::Target { power: 0 });
    }

    #[test]
    fn p6c_parity_piston_extends_pushes_and_retracts_through_engine() {
        // `set_block` powers the piston -> `rs_update_device` queues the
        // block event -> `rs_run_tick` -> `rs_piston_apply` ->
        // `rs_piston_resolve` -> `apply_piston_plan`. The piston flag only
        // flips when the cells actually moved.
        let mut grid = RedstoneSimGrid::default();
        let piston = RsPos(0, 0, 0);
        grid.set_block(
            piston,
            RsBlock::Piston {
                sticky: false,
                facing: RsDir::East,
                extended: false,
            },
        );
        grid.set_block(RsPos(1, 0, 0), RsBlock::Solid);
        grid.set_block(RsPos(0, 0, 1), RsBlock::RedstoneBlock);
        grid.advance(0);
        assert_eq!(
            grid.redstone_block(piston),
            RsBlock::Piston {
                sticky: false,
                facing: RsDir::East,
                extended: true
            }
        );
        assert_eq!(grid.redstone_block(RsPos(1, 0, 0)), RsBlock::Air);
        assert_eq!(grid.redstone_block(RsPos(2, 0, 0)), RsBlock::Solid);

        // Unpower: a non-sticky piston retracts the head but leaves the
        // pushed block where it landed.
        grid.set_block(RsPos(0, 0, 1), RsBlock::Air);
        grid.advance(0);
        assert_eq!(
            grid.redstone_block(piston),
            RsBlock::Piston {
                sticky: false,
                facing: RsDir::East,
                extended: false
            }
        );
        assert_eq!(grid.redstone_block(RsPos(2, 0, 0)), RsBlock::Solid);
    }

    #[test]
    fn p6c_parity_sticky_piston_pulls_pushed_block_back() {
        let mut grid = RedstoneSimGrid::default();
        let piston = RsPos(0, 0, 0);
        grid.set_block(
            piston,
            RsBlock::Piston {
                sticky: true,
                facing: RsDir::East,
                extended: false,
            },
        );
        grid.set_block(RsPos(1, 0, 0), RsBlock::Solid);
        grid.set_block(RsPos(0, 0, 1), RsBlock::RedstoneBlock);
        grid.advance(0);
        assert_eq!(grid.redstone_block(RsPos(2, 0, 0)), RsBlock::Solid);
        // The pushed block is a `moving_piston` for two game ticks — a
        // retraction before it finishes would drop the block instead of
        // pulling it, so the pull path needs the extension to complete.
        grid.advance(2);
        grid.set_block(RsPos(0, 0, 1), RsBlock::Air);
        grid.advance(0);
        // The retract resolver pulls the block from face+2 back onto the head
        // cell — the engine's `to_move` plan, applied by the sim hook.
        assert_eq!(grid.redstone_block(RsPos(1, 0, 0)), RsBlock::Solid);
        assert_eq!(grid.redstone_block(RsPos(2, 0, 0)), RsBlock::Air);
    }

    #[test]
    fn p6c_parity_sensors_route_through_canonical_engine() {
        // Daylight detector: `rs_update_device` -> `rs_daylight_eval` ->
        // `rs_daylight_detector_signal`, rescheduling its own 1-tick refresh.
        let mut grid = RedstoneSimGrid::default();
        let day = RsPos(0, 0, 0);
        grid.daylight_envs.insert(day, (15, 6_000, false, false));
        grid.set_block(
            day,
            RsBlock::DaylightDetector {
                inverted: false,
                power: 0,
            },
        );
        grid.advance(1);
        let RsBlock::DaylightDetector { power, .. } = grid.redstone_block(day) else {
            panic!("daylight detector gone");
        };
        assert_eq!(power, 15, "noon with full sky reads 15 through the engine");

        // Sculk sensor: `sculk_vibration` -> `rs_sculk_sensor_evaluate` ->
        // phase Active, then `rs_run_tick` walks Active -> Cooldown ->
        // Inactive on the canonical delays.
        let sculk = RsPos(4, 0, 0);
        grid.vibrations.insert(
            sculk,
            RsSculkVibration {
                event: "block_place".into(),
                distance: 2.0,
                wool_occluded: false,
            },
        );
        grid.set_block(
            sculk,
            RsBlock::SculkSensor {
                calibrated: false,
                facing: RsDir::North,
                phase: RsSculkPhase::Inactive,
                power: 0,
                waterlogged: false,
            },
        );
        grid.advance(0);
        match grid.redstone_block(sculk) {
            RsBlock::SculkSensor {
                phase: RsSculkPhase::Active,
                power,
                ..
            } => assert!(power > 0),
            other => panic!("sculk sensor should activate, got {other:?}"),
        }
        grid.vibrations.remove(&sculk);
        grid.advance(30);
        match grid.redstone_block(sculk) {
            RsBlock::SculkSensor {
                phase: RsSculkPhase::Cooldown,
                ..
            } => {}
            other => panic!("sculk sensor should cool down, got {other:?}"),
        }
        grid.advance(10);
        match grid.redstone_block(sculk) {
            RsBlock::SculkSensor {
                phase: RsSculkPhase::Inactive,
                ..
            } => {}
            other => panic!("sculk sensor should settle inactive, got {other:?}"),
        }

        // Tripwire hook: `tripwire_line` -> `rs_tripwire_hook_update` writes
        // the cells and the hook flags.
        let hook = RsPos(8, 0, 0);
        grid.tripwire_lines.insert(
            hook,
            RsTripwireResult {
                attached: true,
                powered: true,
                wire_count: 2,
                wires: vec![
                    RsTripwireWireResult {
                        attached: true,
                        powered: true,
                    },
                    RsTripwireWireResult {
                        attached: true,
                        powered: true,
                    },
                ],
            },
        );
        grid.set_block(
            hook,
            RsBlock::TripwireHook {
                facing: RsDir::East,
                attached: false,
                powered: false,
            },
        );
        grid.advance(0);
        assert_eq!(
            grid.redstone_block(hook),
            RsBlock::TripwireHook {
                facing: RsDir::East,
                attached: true,
                powered: true
            }
        );
        assert_eq!(
            grid.tripwire_cells.get(&RsPos(9, 0, 0)),
            Some(&RsTripwireWireResult {
                attached: true,
                powered: true
            })
        );
        assert_eq!(
            grid.tripwire_cells.get(&RsPos(10, 0, 0)),
            Some(&RsTripwireWireResult {
                attached: true,
                powered: true
            })
        );
    }

    #[test]
    fn bedrock_dust_connects_to_observer_on_every_side() {
        // Vanilla Java 1.21.1 `RedStoneWireBlock.shouldConnectTo`: an observer
        // connects dust only when `dir == observer.facing` (dust behind the
        // observer's output face), never on its watched front or side faces.
        let mut grid = RedstoneSimGrid::default();
        grid.set_block(
            RsPos(0, 0, 0),
            RsBlock::Observer {
                facing: RsDir::East,
                powered: false,
            },
        );
        for (pos, dir, expected) in [
            (RsPos(1, 0, 0), RsDir::West, RsWireConnection::None),
            (RsPos(-1, 0, 0), RsDir::East, RsWireConnection::Side),
            (RsPos(0, 0, 1), RsDir::North, RsWireConnection::None),
            (RsPos(0, 0, -1), RsDir::South, RsWireConnection::None),
        ] {
            assert_eq!(
                rs_wire_connection(&grid, pos, dir),
                expected,
                "dust at {pos:?} observer connection mismatch"
            );
        }
        // The observer still emits only out its back (opposite `facing`).
        assert_eq!(
            RsBlock::Observer {
                facing: RsDir::East,
                powered: true
            }
            .weak_signal(RsDir::West),
            15
        );
        assert_eq!(
            RsBlock::Observer {
                facing: RsDir::East,
                powered: true
            }
            .weak_signal(RsDir::East),
            0
        );
    }

    #[test]
    fn bedrock_sticky_piston_one_tick_pulse_drops_block() {
        // The classic sticky-piston pulse: power for a single game tick and
        // the pushed block is left at its destination — the mid-flight
        // `moving_piston` check fires event 2 (drop), not a pull.
        let mut grid = RedstoneSimGrid::default();
        let piston = RsPos(0, 0, 0);
        grid.set_block(
            piston,
            RsBlock::Piston {
                sticky: true,
                facing: RsDir::East,
                extended: false,
            },
        );
        grid.set_block(RsPos(1, 0, 0), RsBlock::Solid);
        grid.set_block(RsPos(0, 0, 1), RsBlock::RedstoneBlock);
        grid.advance(0);
        assert_eq!(
            grid.redstone_block(piston),
            RsBlock::Piston {
                sticky: true,
                facing: RsDir::East,
                extended: true
            }
        );
        assert_eq!(grid.redstone_block(RsPos(2, 0, 0)), RsBlock::Solid);

        // One game tick later the extension is still in flight (progress
        // 0.5 / ticked this tick): unpowering now must drop, not pull.
        grid.advance(1);
        grid.set_block(RsPos(0, 0, 1), RsBlock::Air);
        grid.advance(0);
        assert_eq!(
            grid.redstone_block(piston),
            RsBlock::Piston {
                sticky: true,
                facing: RsDir::East,
                extended: false
            }
        );
        assert_eq!(
            grid.redstone_block(RsPos(2, 0, 0)),
            RsBlock::Solid,
            "the 1-tick pulse leaves the pushed block at its destination"
        );
        assert_eq!(grid.redstone_block(RsPos(1, 0, 0)), RsBlock::Air);
        // And the drop consumed the mid-flight marker: nothing lingers.
        assert!(grid.moving_pistons.is_empty());
    }

    #[test]
    fn bedrock_sticky_piston_zero_tick_pulse_also_drops() {
        // Powered and unpowered within the same game tick — the strongest
        // form of the same bedrock rule (progress < 0.5).
        let mut grid = RedstoneSimGrid::default();
        let piston = RsPos(0, 0, 0);
        grid.set_block(
            piston,
            RsBlock::Piston {
                sticky: true,
                facing: RsDir::East,
                extended: false,
            },
        );
        grid.set_block(RsPos(1, 0, 0), RsBlock::Solid);
        grid.set_block(RsPos(0, 0, 1), RsBlock::RedstoneBlock);
        grid.advance(0);
        grid.set_block(RsPos(0, 0, 1), RsBlock::Air);
        grid.advance(0);
        assert_eq!(grid.redstone_block(RsPos(2, 0, 0)), RsBlock::Solid);
        assert_eq!(grid.redstone_block(RsPos(1, 0, 0)), RsBlock::Air);
        assert!(grid.moving_pistons.is_empty());
    }

    #[test]
    fn bedrock_piston_thirteen_block_line_refuses_to_extend() {
        // `PistonBaseBlock#MAX_PUSH` end-to-end: a powered piston facing a
        // 13-block line stays retracted — the resolver fails and `extended`
        // never flips. At exactly 12 it extends and moves the whole line.
        let mut grid = RedstoneSimGrid::default();
        let piston = RsPos(0, 0, 0);
        grid.set_block(
            piston,
            RsBlock::Piston {
                sticky: false,
                facing: RsDir::East,
                extended: false,
            },
        );
        for x in 1..=13 {
            grid.set_block(RsPos(x, 0, 0), RsBlock::Solid);
        }
        grid.set_block(RsPos(0, 0, 1), RsBlock::RedstoneBlock);
        grid.advance(0);
        assert_eq!(
            grid.redstone_block(piston),
            RsBlock::Piston {
                sticky: false,
                facing: RsDir::East,
                extended: false
            },
            "13 blocks must refuse the block event"
        );
        // Nothing moved either.
        for x in 1..=13 {
            assert_eq!(grid.redstone_block(RsPos(x, 0, 0)), RsBlock::Solid);
        }

        grid.set_block(RsPos(13, 0, 0), RsBlock::Air);
        grid.advance(0);
        assert_eq!(
            grid.redstone_block(piston),
            RsBlock::Piston {
                sticky: false,
                facing: RsDir::East,
                extended: true
            },
            "12 blocks is exactly the vanilla push cap"
        );
        assert_eq!(grid.redstone_block(RsPos(13, 0, 0)), RsBlock::Solid);
    }
}
