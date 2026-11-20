// R7 GP-3: Redstone signal evaluation and scheduled updates.
// Reference: Mojang 1.21.1 server jar (SHA-1 59353fb40c36d304f2035d51e7d6e6baa98dc05c),
// SignalGetter, RedStoneWireBlock, RedstoneTorchBlock, DiodeBlock and their subclasses.

use std::cmp::Reverse;
use std::collections::VecDeque;

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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum RsBlock {
    #[default]
    Air,
    Solid,
    Glass,
    Container(u8),
    RedstoneBlock,
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
    },
    Dispenser {
        triggered: bool,
    },
    Dropper {
        triggered: bool,
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
            Self::Dispenser { .. } => Self::Dispenser { triggered },
            Self::Dropper { .. } => Self::Dropper { triggered },
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
                | Self::Source { .. }
        )
    }

    fn weak_signal(self, toward: RsDir) -> u8 {
        match self {
            Self::RedstoneBlock => 15,
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
            Self::Source { strong, .. } => strong.min(15),
            Self::Wire(_) | Self::Repeater(_) | Self::Comparator(_) => self.weak_signal(toward),
            Self::Torch(_) | Self::WallTorch(_) if toward == RsDir::Up => self.weak_signal(toward),
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
            RsBlock::Hopper { .. } => RsBlock::Hopper {
                enabled: rs_neighbor_signal(self, pos, true) == 0,
            },
            RsBlock::Dispenser { triggered } | RsBlock::Dropper { triggered } => {
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

#[cfg(test)]
mod redstone_tests {
    use super::*;

    const ORIGIN: RsPos = RsPos(0, 0, 0);

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
        grid.set_block(ORIGIN, RsBlock::Dispenser { triggered: false });
        grid.set_block(dropper, RsBlock::Dropper { triggered: false });
        grid.set_block(hopper, RsBlock::Hopper { enabled: true });
        grid.set_block(qc, RsBlock::RedstoneBlock);
        grid.set_block(hopper_input, RsBlock::RedstoneBlock);
        assert_eq!(
            grid.redstone_block(ORIGIN),
            RsBlock::Dispenser { triggered: true }
        );
        assert_eq!(
            grid.redstone_block(dropper),
            RsBlock::Dropper { triggered: true }
        );
        assert_eq!(
            grid.redstone_block(hopper),
            RsBlock::Hopper { enabled: false }
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
            RsBlock::Hopper { enabled: true }
        );
        assert_eq!(
            grid.redstone_block(ORIGIN),
            RsBlock::Dispenser { triggered: false }
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
            grid.set_block(pos, RsBlock::Dispenser { triggered: false });
        }
        grid.schedule(later, 3, RsTickPriority::ExtremelyHigh);
        grid.schedule(second, 2, RsTickPriority::Normal);
        grid.schedule(removed, 1, RsTickPriority::Normal);
        grid.schedule(first, 2, RsTickPriority::High);
        grid.schedule(third, 2, RsTickPriority::Normal);
        grid.set_block(removed, RsBlock::Air);
        grid.set_block(removed, RsBlock::Dispenser { triggered: false });
        grid.advance(3);
        assert_eq!(grid.take_dispense_events(), [first, second, third, later]);
    }
}
