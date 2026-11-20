// R7 GP-3: Redstone signal evaluation and scheduled updates.
// Reference: Mojang 1.21.1 server jar (SHA-1 59353fb40c36d304f2035d51e7d6e6baa98dc05c),
// SignalGetter, RedStoneWireBlock, RedstoneTorchBlock, DiodeBlock and their subclasses.

use std::cmp::Reverse;
use crate::block_registry::BReg;
use std::collections::{BTreeSet as RsBTreeSet, VecDeque};

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

    fn evaluate(self, world: &impl RedstoneWorld, pos: RsPos) -> u8 {
        let rear_pos = pos.relative(self.facing.opposite());
        let mut rear = rs_diode_rear_input(world, pos, self.facing);
        if let Some(analog) = world.container_analog_signal(rear_pos) {
            rear = analog.min(15);
        } else if rear < 15 && world.is_redstone_conductor(rear_pos) {
            if let Some(analog) =
                world.container_analog_signal(rear_pos.relative(self.facing.opposite()))
            {
                rear = analog.min(15);
            }
        }
        let side = rs_side_input(world, pos, self.facing, false);
        match self.mode {
            RsComparatorMode::Compare if rear >= side => rear,
            RsComparatorMode::Compare => 0,
            RsComparatorMode::Subtract => rear.saturating_sub(side),
        }
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RsButtonKind {
    Stone,
    Oak,
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
    Lever { face: RsMountFace, facing: RsDir, powered: bool },
    Button { kind: RsButtonKind, face: RsMountFace, facing: RsDir, powered: bool },
    Piston { sticky: bool, facing: RsDir, extended: bool },
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
                | Self::Source { .. }
        )
    }

    fn weak_signal(self, toward: RsDir) -> u8 {
        match self {
            Self::RedstoneBlock => 15,
            Self::Lever { powered: true, .. } | Self::Button { powered: true, .. } => 15,
            Self::Source { weak, strong } => weak.max(strong).min(15),
            Self::Wire(wire) => wire.signal(toward),
            Self::Torch(torch) if torch.lit && toward != RsDir::Down => 15,
            Self::WallTorch(torch) if torch.lit && toward != torch.facing.opposite() => 15,
            Self::Repeater(repeater) if repeater.powered && toward == repeater.facing => 15,
            Self::Comparator(comparator) if toward == comparator.facing => {
                comparator.output.min(15)
            }
            _ => 0,
        }
    }

    fn direct_signal(self, toward: RsDir) -> u8 {
        match self {
            Self::RedstoneBlock => 15,
            Self::Source { strong, .. } => strong.min(15),
            Self::Lever { face, facing, powered: true }
            | Self::Button { face, facing, powered: true, .. }
                if toward == face.attachment(facing) => 15,
            Self::Wire(_) | Self::Repeater(_) | Self::Comparator(_) => self.weak_signal(toward),
            Self::Torch(_) | Self::WallTorch(_) if toward == RsDir::Up => self.weak_signal(toward),
            _ => 0,
        }
    }
}

// The registry stores the canonical state properties. Keep comparator output separate:
// its analog value lives in a block entity and cannot be recovered from a state ID.
#[cfg_attr(not(test), allow(dead_code))]
fn rs_block_from_state(reg: &BReg, state: u32) -> RsBlock {
    let read = |key: &str| reg.get_prop(state, key);
    let facing = || read("facing").and_then(|v| RsDir::from_property(&v));
    let powered = || read("powered").as_deref() == Some("true");
    let block = (|| -> Option<RsBlock> { match reg.name_of(state) {
        "minecraft:air" if reg.is_air(state) => Some(RsBlock::Air),
        "minecraft:redstone_wire" => Some(RsBlock::Wire(RedStoneWireBlock {
            power: read("power")?.parse().ok()?,
            connections: RsDir::HORIZONTAL.map(|dir| {
                read(dir.property()).and_then(|v| RsWireConnection::from_property(&v))
            }).into_iter().collect::<Option<Vec<_>>>()?.try_into().ok()?,
        })),
        "minecraft:redstone_torch" => Some(RsBlock::Torch(RedstoneTorchBlock {
            lit: read("lit")?.as_str() == "true",
        })),
        "minecraft:redstone_wall_torch" => Some(RsBlock::WallTorch(RedstoneWallTorchBlock {
            facing: facing()?,
            lit: read("lit")?.as_str() == "true",
        })),
        "minecraft:repeater" => Some(RsBlock::Repeater(RepeaterBlock {
            facing: facing()?,
            delay: read("delay")?.parse().ok()?,
            powered: powered(),
            locked: read("locked")?.as_str() == "true",
        })),
        "minecraft:comparator" => {
            let is_powered = powered();
            Some(RsBlock::Comparator(ComparatorBlock {
                facing: facing()?,
                mode: match read("mode")?.as_str() {
                    "compare" => RsComparatorMode::Compare,
                    "subtract" => RsComparatorMode::Subtract,
                    _ => return None,
                },
                powered: is_powered,
                output: if is_powered { 15 } else { 0 },
            }))
        }
        "minecraft:redstone_block" => Some(RsBlock::RedstoneBlock),
        "minecraft:lever" | "minecraft:stone_button" | "minecraft:oak_button" => {
            let face = read("face").and_then(|v| RsMountFace::from_property(&v))?;
            let dir = facing()?;
            let is_powered = powered();
            Some(match reg.name_of(state) {
                "minecraft:lever" => RsBlock::Lever { face, facing: dir, powered: is_powered },
                "minecraft:stone_button" => RsBlock::Button {
                    kind: RsButtonKind::Stone, face, facing: dir, powered: is_powered,
                },
                _ => RsBlock::Button {
                    kind: RsButtonKind::Oak, face, facing: dir, powered: is_powered,
                },
            })
        }
        "minecraft:redstone_lamp" => Some(RsBlock::Lamp {
            lit: read("lit")?.as_str() == "true",
        }),
        "minecraft:hopper" => Some(RsBlock::Hopper {
            enabled: read("enabled")?.as_str() == "true",
            facing: facing()?,
        }),
        "minecraft:piston" | "minecraft:sticky_piston" => Some(RsBlock::Piston {
            sticky: reg.name_of(state) == "minecraft:sticky_piston",
            facing: facing()?,
            extended: read("extended")?.as_str() == "true",
        }),
        "minecraft:dispenser" => Some(RsBlock::Dispenser {
            facing: facing()?,
            triggered: read("triggered")?.as_str() == "true",
        }),
        "minecraft:dropper" => Some(RsBlock::Dropper {
            facing: facing()?,
            triggered: read("triggered")?.as_str() == "true",
        }),
        _ => None,
    } })();
    block.unwrap_or(RsBlock::RegistryState(state))
}

#[cfg_attr(not(test), allow(dead_code))]
fn state_from_rs_block(reg: &BReg, block: &RsBlock) -> Option<u32> {
    let mut properties = Vec::<(String, String)>::new();
    let mut prop = |key: &str, value: String| properties.push((key.into(), value));
    let name = match *block {
        RsBlock::Air => "minecraft:air",
        RsBlock::Solid => "minecraft:stone",
        RsBlock::Glass => "minecraft:glass",
        RsBlock::RegistryState(state) => return reg.properties_of(state).map(|_| state),
        RsBlock::Container(_) | RsBlock::Source { .. } => return None,
        RsBlock::RedstoneBlock => "minecraft:redstone_block",
        RsBlock::Wire(wire) => {
            prop("power", wire.power.to_string());
            for (dir, connection) in RsDir::HORIZONTAL.into_iter().zip(wire.connections) {
                prop(dir.property(), connection.property().into());
            }
            "minecraft:redstone_wire"
        }
        RsBlock::Torch(torch) => {
            prop("lit", torch.lit.to_string());
            "minecraft:redstone_torch"
        }
        RsBlock::WallTorch(torch) => {
            prop("facing", torch.facing.property().into());
            prop("lit", torch.lit.to_string());
            "minecraft:redstone_wall_torch"
        }
        RsBlock::Repeater(repeater) => {
            prop("facing", repeater.facing.property().into());
            prop("delay", repeater.delay.to_string());
            prop("powered", repeater.powered.to_string());
            prop("locked", repeater.locked.to_string());
            "minecraft:repeater"
        }
        RsBlock::Comparator(comparator) => {
            prop("facing", comparator.facing.property().into());
            prop("mode", match comparator.mode {
                RsComparatorMode::Compare => "compare",
                RsComparatorMode::Subtract => "subtract",
            }.into());
            prop("powered", comparator.powered.to_string());
            "minecraft:comparator"
        }
        RsBlock::Lever { face, facing, powered } => {
            prop("face", face.property().into());
            prop("facing", facing.property().into());
            prop("powered", powered.to_string());
            "minecraft:lever"
        }
        RsBlock::Button { kind, face, facing, powered } => {
            prop("face", face.property().into());
            prop("facing", facing.property().into());
            prop("powered", powered.to_string());
            match kind {
                RsButtonKind::Stone => "minecraft:stone_button",
                RsButtonKind::Oak => "minecraft:oak_button",
            }
        }
        RsBlock::Piston { sticky, facing, extended } => {
            prop("facing", facing.property().into());
            prop("extended", extended.to_string());
            if sticky { "minecraft:sticky_piston" } else { "minecraft:piston" }
        }
        RsBlock::Lamp { lit } => {
            prop("lit", lit.to_string());
            "minecraft:redstone_lamp"
        }
        RsBlock::Hopper { enabled, facing } => {
            prop("enabled", enabled.to_string());
            prop("facing", facing.property().into());
            "minecraft:hopper"
        }
        RsBlock::Dispenser { triggered, facing } => {
            prop("triggered", triggered.to_string());
            prop("facing", facing.property().into());
            "minecraft:dispenser"
        }
        RsBlock::Dropper { triggered, facing } => {
            prop("triggered", triggered.to_string());
            prop("facing", facing.property().into());
            "minecraft:dropper"
        }
    };
    reg.checked_state(name, &properties)
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

    fn container_analog_signal(&self, pos: RsPos) -> Option<u8> {
        match self.redstone_block(pos) {
            RsBlock::Container(signal) => Some(signal),
            _ => None,
        }
    }
}

// Signal directions point from the emitting block toward the receiver.
#[cfg_attr(not(test), allow(dead_code))]
fn rs_signal_from(world: &impl RedstoneWorld, pos: RsPos, toward: RsDir, include_wire: bool) -> u8 {
    let block = world.redstone_block(pos);
    let weak = if include_wire || !matches!(block, RsBlock::Wire(_)) {
        block.weak_signal(toward)
    } else {
        0
    };
    if weak == 15 || !world.is_redstone_conductor(pos) {
        return weak;
    }

    // A conductor receives strong power from one neighboring source and emits
    // an effective signal. The lookup never walks through a second conductor.
    let mut signal = weak;
    for dir in RsDir::ALL {
        let source = world.redstone_block(pos.relative(dir));
        if include_wire || !matches!(source, RsBlock::Wire(_)) {
            signal = signal.max(source.direct_signal(dir.opposite()));
            if signal == 15 {
                break;
            }
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
        && world.supports_wire_on_top(side)
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
enum RsTickPriority {
    ExtremelyHigh,
    VeryHigh,
    High,
    Normal,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Default)]
struct RsTorchHistory {
    shutoffs: VecDeque<u64>,
    cooldown_until: u64,
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
        if self.shutoffs.len() >= Self::BURNOUT_THRESHOLD {
            self.cooldown_until = now + Self::COOLDOWN;
            return true;
        }
        false
    }
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
            self.run_tick(pos);
            self.settle();
        }
        self.now = target;
    }

    fn take_dispense_events(&mut self) -> Vec<RsPos> {
        std::mem::take(&mut self.dispense_events)
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

    fn settle(&mut self) {
        self.settle_wires();
        let devices: Vec<_> = self
            .blocks
            .iter()
            .filter_map(|(&pos, &block)| {
                matches!(
                    block,
                    RsBlock::Torch(_)
                        | RsBlock::WallTorch(_)
                        | RsBlock::Repeater(_)
                        | RsBlock::Comparator(_)
                        | RsBlock::Lamp { .. }
                        | RsBlock::Hopper { .. }
                        | RsBlock::Dispenser { .. }
                        | RsBlock::Dropper { .. }
                )
                .then_some((pos, block))
            })
            .collect();
        for (pos, block) in devices {
            self.update_device(pos, block);
        }
    }

    fn settle_wires(&mut self) {
        let mut wires = BTreeMap::new();
        let mut downstream: BTreeMap<RsPos, Vec<RsPos>> = BTreeMap::new();
        let mut queue = VecDeque::new();

        // Seed with non-wire power only. Each relaxation raises a level by at
        // least one, and an unpowered cycle cannot sustain its old levels.
        for (&pos, &block) in &self.blocks {
            let RsBlock::Wire(wire) = block else { continue };
            let mut wire = wire.with_connections(self, pos);
            wire.power = rs_neighbor_signal(self, pos, false);
            if wire.power > 1 {
                queue.push_back(pos);
            }
            wires.insert(pos, wire);
            for input in rs_wire_inputs(self, pos) {
                if matches!(self.redstone_block(input), RsBlock::Wire(_)) {
                    downstream.entry(input).or_default().push(pos);
                }
            }
        }

        while let Some(pos) = queue.pop_front() {
            let candidate = wires[&pos].power.saturating_sub(1);
            if let Some(receivers) = downstream.get(&pos) {
                for receiver in receivers {
                    let wire = wires.get_mut(receiver).expect("wire receiver was indexed");
                    if candidate > wire.power {
                        wire.power = candidate;
                        if candidate > 1 {
                            queue.push_back(*receiver);
                        }
                    }
                }
            }
        }

        for (pos, wire) in wires {
            self.blocks.insert(pos, RsBlock::Wire(wire));
        }
    }

    fn diode_in_front(&self, pos: RsPos, facing: RsDir) -> bool {
        self.redstone_block(pos.relative(facing))
            .diode_facing()
            .is_some_and(|other| other != facing.opposite())
    }

    fn update_device(&mut self, pos: RsPos, block: RsBlock) {
        if let Some((attachment, lit)) = block.torch_attachment() {
            let powered =
                rs_signal_from(self, pos.relative(attachment), attachment.opposite(), true) != 0;
            let cooling = self
                .torches
                .get(&pos)
                .is_some_and(|history| self.now < history.cooldown_until);
            if powered == lit && !cooling {
                self.schedule(pos, RsTorchHistory::DELAY, RsTickPriority::Normal);
            }
            return;
        }

        let updated = match block {
            RsBlock::Repeater(mut repeater) => {
                repeater.locked = repeater.is_locked(self, pos);
                let powered = rs_diode_rear_input(self, pos, repeater.facing) != 0;
                if !repeater.locked && repeater.powered != powered {
                    let priority = if self.diode_in_front(pos, repeater.facing) {
                        RsTickPriority::ExtremelyHigh
                    } else if repeater.powered {
                        RsTickPriority::VeryHigh
                    } else {
                        RsTickPriority::High
                    };
                    self.schedule(pos, repeater.delay_ticks(), priority);
                }
                RsBlock::Repeater(repeater)
            }
            RsBlock::Comparator(comparator) => {
                if comparator.output != comparator.evaluate(self, pos) {
                    let priority = if self.diode_in_front(pos, comparator.facing) {
                        RsTickPriority::High
                    } else {
                        RsTickPriority::Normal
                    };
                    self.schedule(pos, ComparatorBlock::DELAY_TICKS, priority);
                }
                block
            }
            RsBlock::Lamp { lit } => {
                let powered = rs_neighbor_signal(self, pos, true) != 0;
                if lit && !powered {
                    self.schedule(pos, 4, RsTickPriority::Normal);
                }
                RsBlock::Lamp {
                    lit: lit || powered,
                }
            }
            RsBlock::Hopper { facing, .. } => RsBlock::Hopper {
                enabled: rs_neighbor_signal(self, pos, true) == 0,
                facing,
            },
            RsBlock::Dispenser { triggered, .. } | RsBlock::Dropper { triggered, .. } => {
                let powered = rs_dispenser_input(self, pos);
                if !triggered && powered {
                    self.schedule(pos, 4, RsTickPriority::Normal);
                }
                block.with_triggered(powered)
            }
            _ => block,
        };
        if updated != block {
            self.blocks.insert(pos, updated);
        }
    }

    fn run_tick(&mut self, pos: RsPos) {
        let block = self.redstone_block(pos);
        if let Some((attachment, lit)) = block.torch_attachment() {
            let powered =
                rs_signal_from(self, pos.relative(attachment), attachment.opposite(), true) != 0;
            if lit && powered {
                if self
                    .torches
                    .entry(pos)
                    .or_default()
                    .record_shutoff(self.now)
                {
                    self.schedule(pos, RsTorchHistory::COOLDOWN, RsTickPriority::Normal);
                }
                self.blocks.insert(pos, block.with_torch_lit(false));
            } else if !lit
                && !powered
                && !self
                    .torches
                    .get(&pos)
                    .is_some_and(|history| self.now < history.cooldown_until)
            {
                self.blocks.insert(pos, block.with_torch_lit(true));
            }
            return;
        }

        let updated = match block {
            RsBlock::Repeater(mut repeater) if !repeater.is_locked(self, pos) => {
                let input = rs_diode_rear_input(self, pos, repeater.facing) != 0;
                if !repeater.powered && !input {
                    // A queued rising edge still turns on after the input falls.
                    self.schedule(pos, repeater.delay_ticks(), RsTickPriority::VeryHigh);
                }
                repeater.powered = !repeater.powered || input;
                RsBlock::Repeater(repeater)
            }
            RsBlock::Comparator(mut comparator) => {
                comparator.output = comparator.evaluate(self, pos);
                comparator.powered = comparator.output > 0;
                RsBlock::Comparator(comparator)
            }
            RsBlock::Lamp { lit: true } if rs_neighbor_signal(self, pos, true) == 0 => {
                RsBlock::Lamp { lit: false }
            }
            RsBlock::Dispenser { .. } | RsBlock::Dropper { .. } => {
                self.dispense_events.push(pos);
                block
            }
            _ => block,
        };
        if updated != block {
            self.blocks.insert(pos, updated);
        }
    }
}

/// A delayed Redstone transition. The caller owns the pending queue across world ticks.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScheduledRsUpdate {
    position: (i32, i32, i32),
    due_tick: u64,
    priority: RsTickPriority,
}

/// A read-through view of live 1.21.1 state IDs, with batched changes. Reads see
/// pending writes, so wire propagation can settle before the world is notified.
#[cfg_attr(not(test), allow(dead_code))]
struct BRegRedstoneWorld<'a, GetState> {
    reg: &'a BReg,
    get_state: GetState,
    changes: BTreeMap<RsPos, u32>,
    now_tick: u64,
}

#[cfg_attr(not(test), allow(dead_code))]
impl<GetState: Fn((i32, i32, i32)) -> u32> BRegRedstoneWorld<'_, GetState> {
    fn state(&self, pos: RsPos) -> u32 {
        self.changes.get(&pos).copied().unwrap_or_else(|| {
            (self.get_state)((pos.0, pos.1, pos.2))
        })
    }

    fn change(&mut self, pos: RsPos, block: RsBlock) {
        let Some(state) = state_from_rs_block(self.reg, &block) else { return };
        let original = (self.get_state)((pos.0, pos.1, pos.2));
        if state == original {
            self.changes.remove(&pos);
        } else {
            self.changes.insert(pos, state);
        }
    }

    fn commit(self, mut set_state: impl FnMut((i32, i32, i32), u32)) {
        for (RsPos(x, y, z), state) in self.changes {
            set_state((x, y, z), state);
        }
    }

    // Follow the connected dust network in either direction, including elevation
    // steps. A bounded seed neighborhood catches inputs through powered conductors.
    fn wire_network(&self, origins: &[RsPos]) -> RsBTreeSet<RsPos> {
        let mut network = RsBTreeSet::new();
        let mut queue = VecDeque::new();
        for &origin in origins {
            for dx in -2..=2 {
                for dy in -2..=2 {
                    for dz in -2..=2 {
                        let pos = RsPos(origin.0 + dx, origin.1 + dy, origin.2 + dz);
                        if matches!(self.redstone_block(pos), RsBlock::Wire(_))
                            && network.insert(pos)
                        {
                            queue.push_back(pos);
                        }
                    }
                }
            }
        }
        while let Some(pos) = queue.pop_front() {
            let inputs: RsBTreeSet<_> = rs_wire_inputs(self, pos).collect();
            for dir in RsDir::HORIZONTAL {
                let side = pos.relative(dir);
                for dy in -1..=1 {
                    let neighbor = RsPos(side.0, side.1 + dy, side.2);
                    if !matches!(self.redstone_block(neighbor), RsBlock::Wire(_))
                        || network.contains(&neighbor)
                    {
                        continue;
                    }
                    if (inputs.contains(&neighbor)
                        || rs_wire_inputs(self, neighbor).any(|input| input == pos))
                        && network.insert(neighbor)
                    {
                        queue.push_back(neighbor);
                    }
                }
            }
        }
        network
    }

    fn settle_wires(&mut self, network: &RsBTreeSet<RsPos>) {
        let mut wires = BTreeMap::new();
        for &pos in network {
            let RsBlock::Wire(mut wire) = self.redstone_block(pos) else { continue };
            wire.power = 0;
            self.change(pos, RsBlock::Wire(wire));
            wires.insert(pos, wire);
        }

        let mut downstream: BTreeMap<RsPos, Vec<RsPos>> = BTreeMap::new();
        let mut queue = VecDeque::new();
        for (&pos, wire) in &mut wires {
            wire.connections = wire.with_connections(self, pos).connections;
            wire.power = rs_neighbor_signal(self, pos, false);
            if wire.power > 1 {
                queue.push_back(pos);
            }
            for input in rs_wire_inputs(self, pos) {
                if network.contains(&input) {
                    downstream.entry(input).or_default().push(pos);
                }
            }
        }
        while let Some(pos) = queue.pop_front() {
            let power = wires[&pos].power.saturating_sub(1);
            if let Some(receivers) = downstream.get(&pos) {
                for receiver in receivers {
                    let wire = wires.get_mut(receiver).expect("registered wire input");
                    if wire.power < power {
                        wire.power = power;
                        if power > 1 {
                            queue.push_back(*receiver);
                        }
                    }
                }
            }
        }
        for (pos, wire) in wires {
            self.change(pos, RsBlock::Wire(wire));
        }
    }

    fn propagate(&mut self, origins: &[RsPos]) -> Vec<ScheduledRsUpdate> {
        let network = self.wire_network(origins);
        self.settle_wires(&network);
        let mut inspect = RsBTreeSet::new();
        for &origin in origins.iter().chain(network.iter()) {
            for dx in -2..=2 {
                for dy in -2..=2 {
                    for dz in -2..=2 {
                        inspect.insert(RsPos(origin.0 + dx, origin.1 + dy, origin.2 + dz));
                    }
                }
            }
        }

        let mut scheduled = BTreeMap::<RsPos, (u64, RsTickPriority)>::new();
        let now_tick = self.now_tick;
        let mut schedule = |pos: RsPos, delay: u64, priority: RsTickPriority| {
            scheduled.entry(pos).or_insert((now_tick.saturating_add(delay), priority));
        };
        for pos in inspect {
            let block = self.redstone_block(pos);
            if let Some((attachment, lit)) = block.torch_attachment() {
                let powered = rs_signal_from(self, pos.relative(attachment), attachment.opposite(), true) != 0;
                if lit == powered {
                    schedule(pos, RsTorchHistory::DELAY, RsTickPriority::Normal);
                }
                continue;
            }
            match block {
                RsBlock::Repeater(mut repeater) => {
                    repeater.locked = repeater.is_locked(self, pos);
                    if !repeater.locked
                        && repeater.powered != (rs_diode_rear_input(self, pos, repeater.facing) > 0)
                    {
                        let priority = if self.redstone_block(pos.relative(repeater.facing))
                            .diode_facing().is_some_and(|facing| facing != repeater.facing.opposite())
                        {
                            RsTickPriority::ExtremelyHigh
                        } else if repeater.powered {
                            RsTickPriority::VeryHigh
                        } else {
                            RsTickPriority::High
                        };
                        schedule(pos, repeater.delay_ticks(), priority);
                    }
                    self.change(pos, RsBlock::Repeater(repeater));
                }
                RsBlock::Comparator(comparator) => {
                    if (comparator.evaluate(self, pos) > 0) != comparator.powered {
                        schedule(pos, ComparatorBlock::DELAY_TICKS, RsTickPriority::Normal);
                    }
                }
                RsBlock::Lamp { lit } => {
                    let powered = rs_neighbor_signal(self, pos, true) != 0;
                    if lit && !powered {
                        schedule(pos, 4, RsTickPriority::Normal);
                    } else if powered && !lit {
                        self.change(pos, RsBlock::Lamp { lit: true });
                    }
                }
                RsBlock::Hopper { facing, .. } => {
                    let enabled = rs_neighbor_signal(self, pos, true) == 0;
                    self.change(pos, RsBlock::Hopper {
                        enabled, facing,
                    });
                }
                RsBlock::Dispenser { triggered, .. } | RsBlock::Dropper { triggered, .. } => {
                    let powered = rs_dispenser_input(self, pos);
                    if powered && !triggered {
                        schedule(pos, 4, RsTickPriority::Normal);
                    }
                    self.change(pos, block.with_triggered(powered));
                }
                _ => {}
            }
        }
        scheduled.into_iter().map(|(pos, (due_tick, priority))| ScheduledRsUpdate {
            position: (pos.0, pos.1, pos.2), due_tick, priority,
        }).collect()
    }
}

impl<GetState: Fn((i32, i32, i32)) -> u32> RedstoneWorld for BRegRedstoneWorld<'_, GetState> {
    fn redstone_block(&self, pos: RsPos) -> RsBlock {
        rs_block_from_state(self.reg, self.state(pos))
    }

    fn is_redstone_conductor(&self, pos: RsPos) -> bool {
        self.reg.is_redstone_conductor(self.state(pos))
    }

    fn is_face_sturdy(&self, pos: RsPos, face: RsDir) -> bool {
        let direction = RsDir::ALL.iter().position(|&dir| dir == face).expect("native direction") as u8;
        self.reg.is_face_sturdy(self.state(pos), direction, crate::block_registry::Support::Full)
    }
}

/// Propagate changes from block-state origins and return the delayed transitions
/// to enqueue in the host tick scheduler. The reader must include the newly
/// placed state at each origin. All writes are committed after signal evaluation.
#[cfg_attr(not(test), allow(dead_code))]
fn redstone_propagate_states(
    reg: &BReg,
    now_tick: u64,
    origins: &[(i32, i32, i32)],
    get_state: impl Fn((i32, i32, i32)) -> u32,
    set_state: impl FnMut((i32, i32, i32), u32),
) -> Vec<ScheduledRsUpdate> {
    let mut world = BRegRedstoneWorld { reg, get_state, changes: BTreeMap::new(), now_tick };
    let positions: Vec<_> = origins.iter().map(|&(x, y, z)| RsPos(x, y, z)).collect();
    let updates = world.propagate(&positions);
    world.commit(set_state);
    updates
}

/// Apply due host-scheduled transitions and propagate their new block states.
/// Future entries are returned alongside newly scheduled transitions. The host
/// must retain this queue between calls, including pending dispenser/dropper events.
#[cfg_attr(not(test), allow(dead_code))]
fn redstone_apply_scheduled_states(
    reg: &BReg,
    now_tick: u64,
    updates: &[ScheduledRsUpdate],
    get_state: impl Fn((i32, i32, i32)) -> u32,
    set_state: impl FnMut((i32, i32, i32), u32),
) -> Vec<ScheduledRsUpdate> {
    let mut world = BRegRedstoneWorld { reg, get_state, changes: BTreeMap::new(), now_tick };
    let mut pending = Vec::new();
    let mut due: Vec<_> = updates.iter().copied().filter(|tick| tick.due_tick <= now_tick).collect();
    due.sort_by_key(|tick| (tick.due_tick, tick.priority, tick.position));
    let mut changed = Vec::new();
    for tick in due {
        let (x, y, z) = tick.position;
        let pos = RsPos(x, y, z);
        let block = world.redstone_block(pos);
        let updated = if let Some((attachment, _)) = block.torch_attachment() {
            let powered = rs_signal_from(&world, pos.relative(attachment), attachment.opposite(), true) > 0;
            block.with_torch_lit(!powered)
        } else {
            match block {
                RsBlock::Repeater(mut repeater) if !repeater.is_locked(&world, pos) => {
                    let input = rs_diode_rear_input(&world, pos, repeater.facing) > 0;
                    repeater.powered = !repeater.powered || input;
                    RsBlock::Repeater(repeater)
                }
                RsBlock::Comparator(mut comparator) => {
                    comparator.output = comparator.evaluate(&world, pos);
                    comparator.powered = comparator.output > 0;
                    RsBlock::Comparator(comparator)
                }
                RsBlock::Lamp { lit: true } if rs_neighbor_signal(&world, pos, true) == 0 => {
                    RsBlock::Lamp { lit: false }
                }
                _ => block,
            }
        };
        if updated != block {
            world.change(pos, updated);
            changed.push(pos);
        }
    }
    pending.extend(updates.iter().copied().filter(|tick| tick.due_tick > now_tick));
    pending.extend(world.propagate(&changed));
    pending.sort_by_key(|tick| (tick.due_tick, tick.priority, tick.position));
    pending.dedup_by_key(|tick| (tick.position, tick.due_tick));
    world.commit(set_state);
    pending
}

#[cfg(test)]
mod redstone_tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    const ORIGIN: RsPos = RsPos(0, 0, 0);

    fn registered(reg: &BReg, name: &str, properties: &[(&str, &str)]) -> u32 {
        let properties = properties.iter().map(|&(key, value)| (key.into(), value.into()))
            .collect::<Vec<_>>();
        reg.checked_state(name, &properties)
            .unwrap_or_else(|| panic!("missing 1.21.1 state {name} {properties:?}"))
    }

    #[test]
    fn registry_codec_round_trips_every_supported_1211_state() {
        let reg = crate::block_registry::breg().expect("1.21.1 block registry");
        let names = [
            "minecraft:redstone_wire", "minecraft:redstone_torch", "minecraft:redstone_wall_torch",
            "minecraft:repeater", "minecraft:comparator", "minecraft:redstone_block",
            "minecraft:lever", "minecraft:stone_button", "minecraft:oak_button",
            "minecraft:redstone_lamp", "minecraft:hopper", "minecraft:piston",
            "minecraft:sticky_piston", "minecraft:dispenser", "minecraft:dropper",
        ];
        let mut counts = [0usize; 15];
        for raw in 0..reg.state_count() {
            let state = raw as u32;
            let Some(index) = names.iter().position(|&name| name == reg.name_of(state)) else {
                continue;
            };
            let block = rs_block_from_state(reg, state);
            assert!(!matches!(block, RsBlock::RegistryState(_)),
                "{state} {} failed to decode", reg.name_of(state));
            assert_eq!(state_from_rs_block(reg, &block), Some(state),
                "{state} {} {:?}", reg.name_of(state), reg.properties_of(state));
            counts[index] += 1;
        }
        assert!(counts.iter().all(|&count| count != 0), "missing registry types: {counts:?}");
        assert_eq!(counts[0], 16 * 3usize.pow(4), "wire power and connection states");

        let arbitrary = registered(reg, "minecraft:oak_slab", &[("type", "top")]);
        assert_eq!(rs_block_from_state(reg, arbitrary), RsBlock::RegistryState(arbitrary));
        assert_eq!(state_from_rs_block(reg, &RsBlock::RegistryState(arbitrary)), Some(arbitrary));
        assert_eq!(state_from_rs_block(reg, &RsBlock::RegistryState(u32::MAX)), None);
        assert_eq!(state_from_rs_block(reg, &RsBlock::Wire(RedStoneWireBlock {
            power: 16, ..RedStoneWireBlock::default()
        })), None);
    }

    #[test]
    fn registry_conductor_and_support_use_native_state_flags() {
        let reg = crate::block_registry::breg().expect("1.21.1 block registry");
        let stone = registered(reg, "minecraft:stone", &[]);
        let glass = registered(reg, "minecraft:glass", &[]);
        let air = registered(reg, "minecraft:air", &[]);
        let slab = registered(reg, "minecraft:oak_slab", &[("type", "bottom")]);
        let states = HashMap::from([((0, 0, 0), stone), ((1, 0, 0), glass),
            ((2, 0, 0), air), ((3, 0, 0), slab)]);
        let world = BRegRedstoneWorld {
            reg, get_state: |pos| *states.get(&pos).unwrap_or(&air),
            changes: BTreeMap::new(), now_tick: 0,
        };
        assert!(world.is_redstone_conductor(RsPos(0, 0, 0)));
        for x in 1..=3 {
            assert!(!world.is_redstone_conductor(RsPos(x, 0, 0)), "x={x}");
        }
        assert!(world.is_face_sturdy(RsPos(0, 0, 0), RsDir::Up));
        assert!(world.is_face_sturdy(RsPos(1, 0, 0), RsDir::Up));
        assert!(!world.is_face_sturdy(RsPos(2, 0, 0), RsDir::Up));
    }

    #[test]
    fn registry_states_drive_torch_wire_repeater_lamp_circuit() {
        let reg = crate::block_registry::breg().expect("1.21.1 block registry");
        let air = registered(reg, "minecraft:air", &[]);
        let stone = registered(reg, "minecraft:stone", &[]);
        let torch = registered(reg, "minecraft:redstone_torch", &[("lit", "true")]);
        let wire = registered(reg, "minecraft:redstone_wire", &[("power", "0")]);
        let repeater = registered(reg, "minecraft:repeater", &[
            ("facing", "east"), ("delay", "1"), ("powered", "false"),
        ]);
        let lamp = registered(reg, "minecraft:redstone_lamp", &[("lit", "false")]);
        let source = registered(reg, "minecraft:redstone_block", &[]);
        let blocks = Rc::new(RefCell::new(HashMap::from([
            ((0, 0, 0), stone), ((1, 0, 0), stone),
            ((0, 1, 0), torch), ((1, 1, 0), wire),
            ((2, 1, 0), repeater), ((3, 1, 0), lamp),
        ])));
        let propagate = |now_tick, origins: &[(i32, i32, i32)]| {
            let reader = Rc::clone(&blocks);
            let writer = Rc::clone(&blocks);
            redstone_propagate_states(reg, now_tick, origins,
                move |pos| reader.borrow().get(&pos).copied().unwrap_or(air),
                move |pos, state| { writer.borrow_mut().insert(pos, state); })
        };
        let apply = |now_tick, updates: &[ScheduledRsUpdate]| {
            let reader = Rc::clone(&blocks);
            let writer = Rc::clone(&blocks);
            redstone_apply_scheduled_states(reg, now_tick, updates,
                move |pos| reader.borrow().get(&pos).copied().unwrap_or(air),
                move |pos, state| { writer.borrow_mut().insert(pos, state); })
        };
        let at = |pos| blocks.borrow().get(&pos).copied().unwrap_or(air);

        let updates = propagate(0, &[(0, 1, 0), (1, 1, 0), (2, 1, 0), (3, 1, 0)]);
        assert_eq!(reg.get_prop(at((1, 1, 0)), "power").as_deref(), Some("15"));
        assert_eq!(reg.get_prop(at((2, 1, 0)), "powered").as_deref(), Some("false"));
        assert!(updates.iter().any(|update| update.position == (2, 1, 0) && update.due_tick == 2));

        let updates = apply(2, &updates);
        assert_eq!(reg.get_prop(at((2, 1, 0)), "powered").as_deref(), Some("true"));
        assert_eq!(reg.get_prop(at((3, 1, 0)), "lit").as_deref(), Some("true"));

        blocks.borrow_mut().insert((-1, 0, 0), source);
        let updates = [updates, propagate(3, &[(-1, 0, 0)])].concat();
        assert!(updates.iter().any(|update| update.position == (0, 1, 0) && update.due_tick == 5));
        let updates = apply(5, &updates);
        assert_eq!(reg.get_prop(at((0, 1, 0)), "lit").as_deref(), Some("false"));
        assert_eq!(reg.get_prop(at((1, 1, 0)), "power").as_deref(), Some("0"));
        let updates = apply(7, &updates);
        assert_eq!(reg.get_prop(at((2, 1, 0)), "powered").as_deref(), Some("false"));
        assert_eq!(reg.get_prop(at((3, 1, 0)), "lit").as_deref(), Some("true"));
        let _updates = apply(11, &updates);
        assert_eq!(reg.get_prop(at((3, 1, 0)), "lit").as_deref(), Some("false"));
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
        grid.set_block(ORIGIN, RsBlock::Dispenser { triggered: false, facing: RsDir::North });
        grid.set_block(dropper, RsBlock::Dropper { triggered: false, facing: RsDir::North });
        grid.set_block(hopper, RsBlock::Hopper { enabled: true, facing: RsDir::Down });
        grid.set_block(qc, RsBlock::RedstoneBlock);
        grid.set_block(hopper_input, RsBlock::RedstoneBlock);
        assert_eq!(
            grid.redstone_block(ORIGIN),
            RsBlock::Dispenser { triggered: true, facing: RsDir::North }
        );
        assert_eq!(
            grid.redstone_block(dropper),
            RsBlock::Dropper { triggered: true, facing: RsDir::North }
        );
        assert_eq!(
            grid.redstone_block(hopper),
            RsBlock::Hopper { enabled: false, facing: RsDir::Down }
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
            RsBlock::Hopper { enabled: true, facing: RsDir::Down }
        );
        assert_eq!(
            grid.redstone_block(ORIGIN),
            RsBlock::Dispenser { triggered: false, facing: RsDir::North }
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
            grid.set_block(pos, RsBlock::Dispenser { triggered: false, facing: RsDir::North });
        }
        grid.schedule(later, 3, RsTickPriority::ExtremelyHigh);
        grid.schedule(second, 2, RsTickPriority::Normal);
        grid.schedule(removed, 1, RsTickPriority::Normal);
        grid.schedule(first, 2, RsTickPriority::High);
        grid.schedule(third, 2, RsTickPriority::Normal);
        grid.set_block(removed, RsBlock::Air);
        grid.set_block(removed, RsBlock::Dispenser { triggered: false, facing: RsDir::North });
        grid.advance(3);
        assert_eq!(grid.take_dispense_events(), [first, second, third, later]);
    }
}
