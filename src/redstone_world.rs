use crate::block_registry::{BReg, Support};

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RsDaylightDetectorState {
    inverted: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RsTripwireHookState {
    facing: RsDir,
    attached: bool,
    powered: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RsSculkSensorState {
    calibrated: bool,
    phase: RsSculkPhase,
    power: u8,
}

#[cfg_attr(not(test), allow(dead_code))]
fn rs_daylight_detector_state(reg: &BReg, state: u32) -> Option<RsDaylightDetectorState> {
    (reg.name_of(state) == "minecraft:daylight_detector").then(|| RsDaylightDetectorState {
        inverted: reg.get_prop(state, "inverted").as_deref() == Some("true"),
    })
}

#[cfg_attr(not(test), allow(dead_code))]
fn rs_tripwire_cell_from_state(reg: &BReg, state: u32) -> Option<RsTripwireCell> {
    match reg.name_of(state) {
        "minecraft:tripwire" => Some(RsTripwireCell::Wire {
            powered: reg.get_prop(state, "powered").as_deref() == Some("true"),
            disarmed: reg.get_prop(state, "disarmed").as_deref() == Some("true"),
            entity_intersects: false,
        }),
        "minecraft:tripwire_hook" => Some(RsTripwireCell::Hook {
            facing: RsDir::from_property(&reg.get_prop(state, "facing")?)?,
            attached: reg.get_prop(state, "attached").as_deref() == Some("true"),
        }),
        _ => Some(RsTripwireCell::Other),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn rs_tripwire_hook_state(reg: &BReg, state: u32) -> Option<RsTripwireHookState> {
    if reg.name_of(state) != "minecraft:tripwire_hook" {
        return None;
    }
    Some(RsTripwireHookState {
        facing: RsDir::from_property(&reg.get_prop(state, "facing")?)?,
        attached: reg.get_prop(state, "attached").as_deref() == Some("true"),
        powered: reg.get_prop(state, "powered").as_deref() == Some("true"),
    })
}

#[cfg_attr(not(test), allow(dead_code))]
fn rs_sculk_sensor_state(reg: &BReg, state: u32) -> Option<RsSculkSensorState> {
    let name = reg.name_of(state);
    let calibrated = match name {
        "minecraft:sculk_sensor" => false,
        "minecraft:calibrated_sculk_sensor" => true,
        _ => return None,
    };
    Some(RsSculkSensorState {
        calibrated,
        phase: match reg.get_prop(state, "sculk_sensor_phase")?.as_str() {
            "inactive" => RsSculkPhase::Inactive,
            "active" => RsSculkPhase::Active,
            "cooldown" => RsSculkPhase::Cooldown,
            _ => return None,
        },
        power: reg.get_prop(state, "power")?.parse().ok()?,
    })
}

// The registry stores the canonical state properties. Keep comparator output separate:
// its analog value lives in a block entity and cannot be recovered from a state ID.
#[cfg_attr(not(test), allow(dead_code))]
fn rs_block_from_state(reg: &BReg, state: u32) -> RsBlock {
    let read = |key: &str| reg.get_prop(state, key);
    let facing = || read("facing").and_then(|v| RsDir::from_property(&v));
    let powered = || read("powered").as_deref() == Some("true");
    let block = (|| -> Option<RsBlock> {
        match reg.name_of(state) {
            "minecraft:air" if reg.is_air(state) => Some(RsBlock::Air),
            "minecraft:redstone_wire" => Some(RsBlock::Wire(RedStoneWireBlock {
                power: read("power")?.parse().ok()?,
                connections: {
                    let mut connections = [RsWireConnection::None; 4];
                    for (connection, dir) in connections.iter_mut().zip(RsDir::HORIZONTAL) {
                        *connection = RsWireConnection::from_property(&read(dir.property())?)?;
                    }
                    connections
                },
            })),
            "minecraft:redstone_torch" => Some(RsBlock::Torch(RedstoneTorchBlock {
                lit: read("lit")?.as_str() == "true",
            })),
            "minecraft:redstone_wall_torch" => Some(RsBlock::WallTorch(RedstoneWallTorchBlock {
                facing: facing()?,
                lit: read("lit")?.as_str() == "true",
            })),
            "minecraft:repeater" => Some(RsBlock::Repeater(RepeaterBlock {
                // Vanilla FACING points towards the rear input.
                facing: facing()?.opposite(),
                delay: read("delay")?.parse().ok()?,
                powered: powered(),
                locked: read("locked")?.as_str() == "true",
            })),
            "minecraft:comparator" => Some(RsBlock::Comparator(ComparatorBlock {
                facing: facing()?.opposite(),
                mode: match read("mode")?.as_str() {
                    "compare" => RsComparatorMode::Compare,
                    "subtract" => RsComparatorMode::Subtract,
                    _ => return None,
                },
                powered: powered(),
                output: 0,
            })),
            "minecraft:redstone_block" => Some(RsBlock::RedstoneBlock),
            "minecraft:lever" => Some(RsBlock::Lever {
                face: RsMountFace::from_property(&read("face")?)?,
                facing: facing()?,
                powered: powered(),
            }),
            "minecraft:observer" => Some(RsBlock::Observer {
                facing: facing()?,
                powered: powered(),
            }),
            "minecraft:target" => Some(RsBlock::Target {
                power: read("power")?.parse().ok()?,
            }),
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
            _ if reg.in_tag(state, "minecraft:buttons") => Some(RsBlock::Button {
                block_id: reg.block_id_of(state)? as u32,
                face: RsMountFace::from_property(&read("face")?)?,
                facing: facing()?,
                powered: powered(),
            }),
            _ => None,
        }
    })();
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
            prop("facing", repeater.facing.opposite().property().into());
            prop("delay", repeater.delay.to_string());
            prop("powered", repeater.powered.to_string());
            prop("locked", repeater.locked.to_string());
            "minecraft:repeater"
        }
        RsBlock::Comparator(comparator) => {
            prop("facing", comparator.facing.opposite().property().into());
            prop(
                "mode",
                match comparator.mode {
                    RsComparatorMode::Compare => "compare",
                    RsComparatorMode::Subtract => "subtract",
                }
                .into(),
            );
            prop("powered", comparator.powered.to_string());
            "minecraft:comparator"
        }
        RsBlock::Lever {
            face,
            facing,
            powered,
        }
        | RsBlock::Button {
            face,
            facing,
            powered,
            ..
        } => {
            prop("face", face.property().into());
            prop("facing", facing.property().into());
            prop("powered", powered.to_string());
            if let RsBlock::Button { block_id, .. } = *block {
                let state = reg.default_state(block_id);
                if !reg.in_tag(state, "minecraft:buttons") {
                    return None;
                }
                reg.name_of(state)
            } else {
                "minecraft:lever"
            }
        }
        RsBlock::Observer { facing, powered } => {
            prop("facing", facing.property().into());
            prop("powered", powered.to_string());
            "minecraft:observer"
        }
        RsBlock::Target { power } => {
            prop("power", power.to_string());
            "minecraft:target"
        }
        RsBlock::Piston {
            sticky,
            facing,
            extended,
        } => {
            prop("facing", facing.property().into());
            prop("extended", extended.to_string());
            if sticky {
                "minecraft:sticky_piston"
            } else {
                "minecraft:piston"
            }
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RsWorldTick {
    pos: RsPos,
    block_id: u32,
    delay: u64,
    priority: RsTickPriority,
}

/// A single dimension's block and block-entity storage, notification queue and
/// block tick scheduler. Callers drain queued notifications outside the write.
#[cfg_attr(not(test), allow(dead_code))]
trait RedstoneBlockWorld {
    fn state_at(&self, pos: RsPos) -> u32;
    fn is_redstone_conductor(&self, pos: RsPos) -> bool;
    fn comparator_output(&self, pos: RsPos) -> u8;
    fn analog_signal(&self, pos: RsPos) -> Option<u8>;

    /// Return the analog signal of exactly one item frame facing this direction.
    fn item_frame_signal(&self, _pos: RsPos, _facing: RsDir) -> Option<u8> {
        None
    }

    /// Apply the block state and any new comparator output before queuing notifications.
    /// An unchanged state and output can still request a comparator output notification.
    fn apply_redstone_change(&mut self, pos: RsPos, state: u32, comparator_output: Option<u8>);
    /// Deduplicate pending ticks by position and raw block id. Use the world
    /// scheduler's ordering and remove the request from pending before dispatch.
    fn schedule_redstone_tick(&mut self, tick: RsWorldTick);
    fn dispense_redstone(&mut self, pos: RsPos);
}

#[cfg_attr(not(test), allow(dead_code))]
struct RedstoneWorldAdapter<'a, W> {
    reg: &'a BReg,
    world: &'a mut W,
    torches: &'a mut BTreeMap<RsPos, RsTorchHistory>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl<'a, W: RedstoneBlockWorld> RedstoneWorldAdapter<'a, W> {
    fn new(
        reg: &'a BReg,
        world: &'a mut W,
        torches: &'a mut BTreeMap<RsPos, RsTorchHistory>,
    ) -> Self {
        Self {
            reg,
            world,
            torches,
        }
    }

    // Follow the connected dust network in either direction, including elevation
    // steps. A bounded seed neighborhood catches inputs through powered conductors.
    fn wire_network(&self, seeds: &RsBTreeSet<RsPos>) -> RsBTreeSet<RsPos> {
        let mut network = RsBTreeSet::new();
        let mut queue = VecDeque::new();
        for &pos in seeds {
            if matches!(self.redstone_block(pos), RsBlock::Wire(_)) {
                network.insert(pos);
                queue.push_back(pos);
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
                    if inputs.contains(&neighbor)
                        || rs_wire_inputs(self, neighbor).any(|input| input == pos)
                    {
                        network.insert(neighbor);
                        queue.push_back(neighbor);
                    }
                }
            }
        }
        network
    }

    /// Settle dust touched by the changed positions before evaluating nearby devices.
    /// The host supplies the new origin states and owns the queued notifications and ticks.
    fn propagate(&mut self, origins: &[RsPos]) {
        let mut inspect = RsBTreeSet::new();
        rs_extend_neighborhood(&mut inspect, origins.iter().copied());
        let network = self.wire_network(&inspect);
        rs_settle_wires(self, network.iter().copied());
        rs_extend_neighborhood(&mut inspect, network.into_iter());
        for pos in inspect {
            let block = self.redstone_block(pos);
            rs_update_device(self, pos, block);
        }
    }

    /// Refresh dust geometry when its surroundings change.
    fn refresh_wire_shape(&mut self, pos: RsPos) {
        if let RsBlock::Wire(wire) = self.redstone_block(pos) {
            let updated = wire.with_connections(self, pos);
            if updated != wire {
                self.set_component(pos, RsBlock::Wire(updated));
            }
        }
    }

    /// Evaluate the receiver of a neighbor notification.
    fn neighbor_changed(&mut self, pos: RsPos) {
        let block = self.redstone_block(pos);
        if let RsBlock::Wire(wire) = block {
            let direct = rs_neighbor_signal(self, pos, false);
            let power = if direct == 15 {
                direct
            } else {
                let wire_input = rs_wire_inputs(self, pos)
                    .filter_map(|input| match self.redstone_block(input) {
                        RsBlock::Wire(wire) => Some(wire.power),
                        _ => None,
                    })
                    .max()
                    .unwrap_or(0);
                direct.max(wire_input.saturating_sub(1))
            };
            if power != wire.power {
                self.set_component(pos, RsBlock::Wire(RedStoneWireBlock { power, ..wire }));
            }
        } else {
            rs_update_device(self, pos, block);
        }
    }

    fn run_scheduled_tick(&mut self, pos: RsPos, block_id: u32, now: u64) {
        if self
            .reg
            .block_id_of(self.world.state_at(pos))
            .map(|id| id as u32)
            == Some(block_id)
        {
            rs_run_tick(self, pos, now);
        }
    }
}

impl<W: RedstoneBlockWorld> RedstoneWorld for RedstoneWorldAdapter<'_, W> {
    fn redstone_block(&self, pos: RsPos) -> RsBlock {
        match rs_block_from_state(self.reg, self.world.state_at(pos)) {
            RsBlock::Comparator(comparator) => RsBlock::Comparator(ComparatorBlock {
                output: self.world.comparator_output(pos),
                ..comparator
            }),
            block => block,
        }
    }

    fn is_redstone_conductor(&self, pos: RsPos) -> bool {
        self.world.is_redstone_conductor(pos)
    }

    fn is_face_sturdy(&self, pos: RsPos, face: RsDir) -> bool {
        self.reg
            .is_face_sturdy(self.world.state_at(pos), face as u8, Support::Full)
    }

    fn allows_wire_step_up(&self, pos: RsPos) -> bool {
        self.supports_wire_on_top(pos)
            || self
                .reg
                .in_tag(self.world.state_at(pos), "minecraft:trapdoors")
    }

    fn container_analog_signal(&self, pos: RsPos) -> Option<u8> {
        self.world.analog_signal(pos)
    }

    fn item_frame_signal(&self, pos: RsPos, facing: RsDir) -> Option<u8> {
        self.world.item_frame_signal(pos, facing)
    }
}

impl<W: RedstoneBlockWorld> RedstoneUpdates for RedstoneWorldAdapter<'_, W> {
    fn set_component(&mut self, pos: RsPos, block: RsBlock) {
        let state = self.world.state_at(pos);
        let updated = state_from_rs_block(self.reg, &block)
            .expect("redstone update must encode valid registry properties");
        assert_eq!(self.reg.block_id_of(state), self.reg.block_id_of(updated));
        let output = match block {
            RsBlock::Comparator(comparator)
                if comparator.output != self.world.comparator_output(pos) =>
            {
                Some(comparator.output)
            }
            _ => None,
        };
        if updated != state || matches!(block, RsBlock::Comparator(_)) {
            self.world.apply_redstone_change(pos, updated, output);
        }
    }

    fn schedule(&mut self, pos: RsPos, delay: u64, priority: RsTickPriority) {
        if let Some(block_id) = self.reg.block_id_of(self.world.state_at(pos)) {
            self.world.schedule_redstone_tick(RsWorldTick {
                pos,
                block_id: block_id as u32,
                delay,
                priority,
            });
        }
    }

    fn torch_histories(&mut self) -> &mut BTreeMap<RsPos, RsTorchHistory> {
        self.torches
    }

    fn dispense(&mut self, pos: RsPos) {
        self.world.dispense_redstone(pos);
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn rs_extend_neighborhood(
    positions: &mut RsBTreeSet<RsPos>,
    origins: impl IntoIterator<Item = RsPos>,
) {
    for RsPos(x, y, z) in origins {
        for dx in -2..=2 {
            for dy in -2..=2 {
                for dz in -2..=2 {
                    positions.insert(RsPos(x + dx, y + dy, z + dz));
                }
            }
        }
    }
}
