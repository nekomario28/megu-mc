use crate::block_registry::{BReg, Support};

#[cfg(all(test, megu_vanilla_data))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RsDaylightDetectorState {
    inverted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RsTripwireHookState {
    facing: RsDir,
    attached: bool,
    powered: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RsSculkSensorState {
    calibrated: bool,
    phase: RsSculkPhase,
    power: u8,
}

#[cfg(all(test, megu_vanilla_data))]
fn rs_daylight_detector_state(reg: &BReg, state: u32) -> Option<RsDaylightDetectorState> {
    (reg.name_of(state) == "minecraft:daylight_detector").then(|| RsDaylightDetectorState {
        inverted: reg.get_prop(state, "inverted").as_deref() == Some("true"),
    })
}

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
            "minecraft:daylight_detector" => Some(RsBlock::DaylightDetector {
                inverted: read("inverted")?.as_str() == "true",
                power: read("power")?.parse().ok()?,
            }),
            "minecraft:tripwire_hook" => {
                let hook = rs_tripwire_hook_state(reg, state)?;
                Some(RsBlock::TripwireHook {
                    facing: hook.facing,
                    attached: hook.attached,
                    powered: hook.powered,
                })
            }
            "minecraft:sculk_sensor" | "minecraft:calibrated_sculk_sensor" => {
                let sensor = rs_sculk_sensor_state(reg, state)?;
                Some(RsBlock::SculkSensor {
                    calibrated: sensor.calibrated,
                    facing: facing().unwrap_or(RsDir::North),
                    phase: sensor.phase,
                    power: sensor.power,
                    waterlogged: read("waterlogged")?.as_str() == "true",
                })
            }
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

/// The `piston_head` state an extension leaves at the head cell:
/// `facing`/`short=false`/`type` from the moving piston (`short` marks the
/// retracted head variant, which a live extension never writes).
fn piston_head_state(reg: &BReg, facing: RsDir, sticky: bool) -> Option<u32> {
    reg.checked_state(
        "minecraft:piston_head",
        &[
            ("facing".to_string(), facing.property().to_string()),
            ("short".to_string(), "false".to_string()),
            (
                "type".to_string(),
                if sticky { "sticky" } else { "normal" }.to_string(),
            ),
        ],
    )
}

fn state_from_rs_block(reg: &BReg, block: &RsBlock) -> Option<u32> {
    let mut properties = Vec::<(String, String)>::new();
    let mut prop = |key: &str, value: String| properties.push((key.into(), value));
    let name = match *block {
        RsBlock::Air => "minecraft:air",
        #[cfg(test)]
        RsBlock::Solid => "minecraft:stone",
        #[cfg(test)]
        RsBlock::Glass => "minecraft:glass",
        RsBlock::RegistryState(state) => return reg.properties_of(state).map(|_| state),
        #[cfg(test)]
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
        RsBlock::DaylightDetector { inverted, power } => {
            prop("inverted", inverted.to_string());
            prop("power", power.to_string());
            "minecraft:daylight_detector"
        }
        RsBlock::TripwireHook {
            facing,
            attached,
            powered,
        } => {
            prop("attached", attached.to_string());
            prop("facing", facing.property().into());
            prop("powered", powered.to_string());
            "minecraft:tripwire_hook"
        }
        RsBlock::SculkSensor {
            calibrated,
            facing,
            phase,
            power,
            waterlogged,
        } => {
            if calibrated {
                prop("facing", facing.property().into());
            }
            prop("power", power.to_string());
            prop(
                "sculk_sensor_phase",
                match phase {
                    RsSculkPhase::Inactive => "inactive",
                    RsSculkPhase::Active => "active",
                    RsSculkPhase::Cooldown => "cooldown",
                }
                .into(),
            );
            prop("waterlogged", waterlogged.to_string());
            if calibrated {
                "minecraft:calibrated_sculk_sensor"
            } else {
                "minecraft:sculk_sensor"
            }
        }
    };
    reg.checked_state(name, &properties)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RsWorldTick {
    pos: RsPos,
    block_id: u32,
    delay: u64,
    priority: RsTickPriority,
}

/// A world view that can name the dimension it samples. `RedstoneBlockWorld`
/// implementors outside this file (the live dimensions) implement it
/// separately so mid-flight piston marks and build-height bounds key per
/// dimension; dimension-agnostic views keep the `None` default.
trait RsWorldDim {
    fn dim_id(&self) -> Option<crate::worldgen::DimId> {
        None
    }
}

/// The live dimension views name their dimension so piston flight markers and
/// build-height bounds stay per-dimension.
impl RsWorldDim for LiveRedstoneWorld<'_> {
    fn dim_id(&self) -> Option<crate::worldgen::DimId> {
        Some(self.dim)
    }
}

/// A single dimension's block and block-entity storage, notification queue and
/// block tick scheduler. Callers drain queued notifications outside the write.
trait RedstoneBlockWorld: RsWorldDim {
    fn state_at(&self, pos: RsPos) -> u32;
    fn is_redstone_conductor(&self, pos: RsPos) -> bool;
    fn comparator_output(&self, pos: RsPos) -> u8;
    fn analog_signal(&self, pos: RsPos) -> Option<u8>;

    /// Return the analog signal of exactly one item frame facing this direction.
    fn item_frame_signal(&self, _pos: RsPos, _facing: RsDir) -> Option<u8> {
        None
    }

    /// `(sky_light, day_time, raining, thundering)` observed above a daylight
    /// detector, or `None` when the host cannot sample the sky; `None` keeps the
    /// stored `power` property authoritative.
    fn daylight_env(&self, _pos: RsPos) -> Option<(u8, i64, bool, bool)> {
        None
    }

    /// Whether an entity bounding box intersects the tripwire cell at `pos`.
    fn entity_intersects(&self, _pos: RsPos) -> bool {
        false
    }

    /// The vibration a sculk sensor at `pos` would hear, or `None`.
    fn sculk_vibration(&self, _pos: RsPos) -> Option<RsSculkVibration> {
        None
    }

    /// Apply the block state and any new comparator output before queuing notifications.
    /// An unchanged state and output can still request a comparator output notification.
    fn apply_redstone_change(&mut self, pos: RsPos, state: u32, comparator_output: Option<u8>);
    /// Deduplicate pending ticks by position and raw block id. Use the world
    /// scheduler's ordering and remove the request from pending before dispatch.
    fn schedule_redstone_tick(&mut self, tick: RsWorldTick);
    fn dispense_redstone(&mut self, pos: RsPos);

    /// A `moving_piston` block entity still in flight at `pos`, read by
    /// `rs_piston_retraction_event` for the one-tick-pulse drop rule. `now`
    /// is the game tick the caller evaluates at.
    fn moving_piston_at(&self, pos: RsPos, now: u64) -> Option<RsMovingPiston> {
        live_piston_marks()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .moving_at(self.dim_id(), pos, now)
    }

    /// Record the mid-flight marker an extension leaves at a moved cell.
    fn mark_moving_piston(&mut self, pos: RsPos, now: u64) {
        live_piston_marks()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mark(self.dim_id(), pos, now);
    }

    /// Drop the mid-flight marker (the one-tick-pulse "spit out" case, or a
    /// retraction that actually pulls the block home).
    fn clear_moving_piston(&mut self, pos: RsPos) {
        live_piston_marks()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear(self.dim_id(), pos);
    }
}

/// Per-position `moving_piston` flight markers for worlds that keep no piston
/// state of their own (the live dimensions). Vanilla stores these on the
/// `moving_piston` block entity for the two game ticks a pushed cell is in
/// flight; the table expires entries lazily on read and eagerly on mark.
///
/// Keyed by `(dimension, position)`: same-coordinate pistons in the
/// Overworld and the Nether must never share a mark. Views that cannot name
/// their dimension share the `None` bucket — the pre-dimension behaviour.
#[derive(Default)]
struct LivePistonMarks {
    moving: BTreeMap<(Option<crate::worldgen::DimId>, RsPos), RsMovingPiston>,
}

fn live_piston_marks() -> &'static Mutex<LivePistonMarks> {
    static MARKS: OnceLock<Mutex<LivePistonMarks>> = OnceLock::new();
    MARKS.get_or_init(|| Mutex::new(LivePistonMarks::default()))
}

impl LivePistonMarks {
    /// `PistonMovingBlockEntity.tick` catch-up: `progress += 0.5` per elapsed
    /// game tick; the entity is gone once progress reaches 1.0.
    fn moving_at(
        &mut self,
        dim: Option<crate::worldgen::DimId>,
        pos: RsPos,
        now: u64,
    ) -> Option<RsMovingPiston> {
        let key = (dim, pos);
        let moving = self.moving.get_mut(&key)?;
        if moving.last_ticked < now {
            moving.progress += 0.5 * (now - moving.last_ticked) as f32;
            moving.last_ticked = now;
        }
        if moving.progress >= 1.0 {
            self.moving.remove(&key);
            return None;
        }
        Some(*moving)
    }

    fn mark(&mut self, dim: Option<crate::worldgen::DimId>, pos: RsPos, now: u64) {
        self.moving
            .retain(|_, m| now.saturating_sub(m.last_ticked) < 4);
        self.moving.insert(
            (dim, pos),
            RsMovingPiston {
                progress: 0.0,
                last_ticked: now,
            },
        );
    }

    fn clear(&mut self, dim: Option<crate::worldgen::DimId>, pos: RsPos) {
        self.moving.remove(&(dim, pos));
    }
}

struct RedstoneWorldAdapter<'a, W> {
    reg: &'a BReg,
    world: &'a mut W,
    torches: &'a mut BTreeMap<RsPos, RsTorchHistory>,
    build_height: (i32, i32),
    /// Pistons whose block event failed to move cells during this adapter's
    /// pass — vanilla consumes a `PistonBaseBlock` event once per
    /// `neighborChanged` wave, so the mark lives exactly as long as the
    /// adapter (one update wave) does.
    consumed_piston_events: BTreeSet<RsPos>,
}

fn redstone_build_height_from_dimension(dim: crate::worldgen::DimId) -> (i32, i32) {
    let min_y = dim.min_y();
    (min_y, min_y + dim.height())
}

impl<'a, W: RedstoneBlockWorld> RedstoneWorldAdapter<'a, W> {
    fn new(
        reg: &'a BReg,
        world: &'a mut W,
        torches: &'a mut BTreeMap<RsPos, RsTorchHistory>,
    ) -> Self {
        // Vanilla `Level.getMinBuildHeight`/`getMaxBuildHeight` are per
        // dimension; views that cannot name theirs keep the Overworld span.
        let build_height = world
            .dim_id()
            .map_or((-64, 320), redstone_build_height_from_dimension);
        Self::new_with_build_height(build_height, reg, world, torches)
    }

    fn new_with_build_height(
        build_height: (i32, i32),
        reg: &'a BReg,
        world: &'a mut W,
        torches: &'a mut BTreeMap<RsPos, RsTorchHistory>,
    ) -> Self {
        Self {
            reg,
            world,
            torches,
            build_height,
            consumed_piston_events: BTreeSet::new(),
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
        rs_extend_neighborhood(&mut inspect, network);
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

    /// Block-state analog families the host's `analog_signal` hook does not
    /// own: cake, composter, cauldrons, end portal frames, respawn anchors,
    /// beehives and the copper-bulb oxidation variants — every one overrides
    /// `hasAnalogOutputSignal` in 1.21.1, so an empty source still reports
    /// `Some`, which correctly suppresses the through-conductor read.
    /// `hasAnalogOutputSignal` block-entity families with no live record read
    /// 0, exactly like vanilla's failed `instanceof` guard. Detector rails,
    /// sculk sensors and candle cakes also carry analog output but need data
    /// this view cannot see; they keep the `None` fallthrough.
    fn registry_analog_signal(&self, pos: RsPos) -> Option<u8> {
        let state = self.world.state_at(pos);
        let name = self.reg.name_of(state);
        let path = name.rsplit_once(':').map_or(name, |(_, path)| path);
        let level = |key: &str| -> Option<u8> { self.reg.get_prop(state, key)?.parse().ok() };
        let truthy = |key: &str| self.reg.get_prop(state, key).as_deref() == Some("true");
        let source = match path {
            "cake" => RsAnalogSource::Cake { bites: level("bites")? },
            "composter" => RsAnalogSource::Composter {
                level: level("level")?,
            },
            "water_cauldron" | "powder_snow_cauldron" => RsAnalogSource::LayeredCauldron {
                level: level("level")?,
            },
            "lava_cauldron" => RsAnalogSource::LavaCauldron,
            "end_portal_frame" => RsAnalogSource::EndPortalFrame { eye: truthy("eye") },
            "respawn_anchor" => RsAnalogSource::RespawnAnchor {
                charges: level("charges")?,
            },
            "beehive" | "bee_nest" => RsAnalogSource::Beehive {
                honey_level: level("honey_level")?,
            },
            _ if path.ends_with("copper_bulb") => RsAnalogSource::CopperBulb {
                lit: truthy("lit"),
            },
            // Block-entity families with no live record read their vanilla
            // empty-source value: no book, no disc, no last slot, an empty
            // container. `crafter` and `brewing_stand` have no block-state
            // model here at all, so they keep the constant-0 answer.
            "lectern" => RsAnalogSource::Lectern {
                has_book: false,
                page: 0,
                page_count: 0,
            },
            "jukebox" => RsAnalogSource::Jukebox { song_output: None },
            "chiseled_bookshelf" => RsAnalogSource::ChiseledBookshelf { last_slot: None },
            "decorated_pot" => RsAnalogSource::DecoratedPot {
                count: 0,
                max_stack: 1,
            },
            "command_block" | "chain_command_block" | "repeating_command_block" => {
                RsAnalogSource::CommandBlock { success_count: 0 }
            }
            "crafter" | "brewing_stand" => return Some(0),
            "shulker_box" | "chest" | "trapped_chest" | "barrel" | "dispenser" | "dropper"
            | "hopper" | "furnace" | "blast_furnace" | "smoker" => {
                RsAnalogSource::Container {
                    slots: Vec::new(),
                    num_slots: 1,
                }
            }
            _ => return None,
        };
        Some(rs_comparator_signal(&source))
    }

    /// Evaluate the receiver of a neighbor notification.
    #[cfg(all(test, megu_vanilla_data))]
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
    fn build_height(&self) -> (i32, i32) {
        self.build_height
    }

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
        self.world
            .analog_signal(pos)
            .or_else(|| self.registry_analog_signal(pos))
    }

    fn item_frame_signal(&self, pos: RsPos, facing: RsDir) -> Option<u8> {
        self.world.item_frame_signal(pos, facing)
    }

    fn daylight_env(&self, pos: RsPos) -> Option<(u8, i64, bool, bool)> {
        self.world.daylight_env(pos)
    }

    fn tripwire_line(&self, pos: RsPos, facing: RsDir) -> Option<RsTripwireResult> {
        let mut cells = Vec::new();
        let mut cursor = pos;
        for _ in 0..41 {
            cursor = cursor.relative(facing);
            let mut cell = rs_tripwire_cell_from_state(self.reg, self.world.state_at(cursor))?;
            if let RsTripwireCell::Wire {
                entity_intersects, ..
            } = &mut cell
            {
                *entity_intersects = self.world.entity_intersects(cursor);
            }
            let end = !matches!(cell, RsTripwireCell::Wire { .. });
            cells.push(cell);
            if end {
                break;
            }
        }
        Some(rs_tripwire_line(facing, &cells))
    }

    fn sculk_vibration(&self, pos: RsPos) -> Option<RsSculkVibration> {
        self.world.sculk_vibration(pos)
    }

    /// `PistonBaseBlock.isPushable` over real registry states: modeled blocks
    /// keep their `RsBlock` answer, everything else classifies by its 1.21.1
    /// name, destroy speed and `EntityBlock` flag — the data that makes
    /// ordinary blocks (stone, dirt, planks, wool) pushable and chests,
    /// obsidian and command blocks immovable.
    fn piston_push_block(&self, pos: RsPos) -> RsPushBlock {
        match self.redstone_block(pos) {
            RsBlock::RegistryState(state) => rs_push_block_of_state(self.reg, state),
            block => rs_push_block_of(block),
        }
    }

    fn moving_piston_at(&self, pos: RsPos, now: u64) -> Option<RsMovingPiston> {
        self.world.moving_piston_at(pos, now)
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

    fn update_tripwire_cell(&mut self, pos: RsPos, wire: RsTripwireWireResult) {
        let state = self.world.state_at(pos);
        if self.reg.name_of(state) != "minecraft:tripwire" {
            return;
        }
        let updated = self.reg.with_prop(
            self.reg
                .with_prop(state, "attached", &wire.attached.to_string()),
            "powered",
            &wire.powered.to_string(),
        );
        if updated != state {
            self.world.apply_redstone_change(pos, updated, None);
        }
    }

    fn piston_event_consumed(&self, pos: RsPos) -> bool {
        self.consumed_piston_events.contains(&pos)
    }

    fn consume_piston_event(&mut self, pos: RsPos) {
        self.consumed_piston_events.insert(pos);
    }

    /// `PistonBaseBlock.triggerEvent` applied to real registry states. Every
    /// moved cell keeps its exact state id (properties included); the head
    /// cell gets air on retract or a `piston_head` state on extend, and each
    /// cell an extension displaces is stamped as a `moving_piston` flight
    /// marker for the sticky-drop rule.
    fn apply_piston_plan(
        &mut self,
        pos: RsPos,
        facing: RsDir,
        extending: bool,
        plan: &RsPistonPlan,
        drop_pulled: bool,
        now: u64,
    ) -> bool {
        let push_dir = if extending { facing } else { facing.opposite() };
        let air = state_from_rs_block(self.reg, &RsBlock::Air)
            .unwrap_or_else(|| self.reg.state_of("air", None));
        // Collect every moved state before any origin clears, like the sim:
        // a contiguous line cannot clobber itself mid-shift.
        let moved: Vec<(RsPos, u32)> = if drop_pulled {
            Vec::new()
        } else {
            plan.to_move
                .iter()
                .map(|&cell| (cell.steps(push_dir, 1), self.world.state_at(cell)))
                .collect()
        };
        let head = pos.relative(facing);
        self.world.apply_redstone_change(head, air, None);
        for &cell in &plan.to_destroy {
            self.world.apply_redstone_change(cell, air, None);
        }
        if drop_pulled {
            // Event 2 leaves the pushed block where it landed: the cells stay
            // put and the mid-flight markers are consumed.
            for &cell in &plan.to_move {
                self.world.clear_moving_piston(cell);
            }
        } else {
            for &cell in &plan.to_move {
                self.world.apply_redstone_change(cell, air, None);
            }
            for (dst, state) in moved {
                self.world.apply_redstone_change(dst, state, None);
                if extending {
                    self.world.mark_moving_piston(dst, now);
                }
            }
            if extending {
                let sticky = matches!(
                    self.redstone_block(pos),
                    RsBlock::Piston { sticky: true, .. }
                );
                if let Some(head_state) = piston_head_state(self.reg, facing, sticky) {
                    self.world.apply_redstone_change(head, head_state, None);
                }
            }
        }
        true
    }
}

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

/// `Level.updateNeighbourForOutputSignal(BlockPos, Block)` for live
/// dimensions: an analog-output block entity changed without a block-state
/// write (container contents, lectern page, jukebox disc, crafter slot
/// toggle, copper-bulb-style state flips handled elsewhere), so no redstone
/// pass was queued. Vanilla calls this from `BlockEntity.setChanged` and
/// from `Level.setBlock` whenever the touched state `hasAnalogOutputSignal`;
/// the engine half wakes comparators on the four horizontal neighbors —
/// and one conductor deeper — while the scheduled comparator tick does the
/// rest of the neighbor update two game ticks later.
///
/// Consumed by the container and block-entity mutation sites; until those
/// lanes wire their calls this entry point is intentionally unused here.
#[allow(dead_code)]
pub(crate) fn update_neighbour_for_output_signal(dim: crate::worldgen::DimId, pos: (i32, i32, i32)) {
    let Some(reg) = breg() else { return };
    let origin = RsPos(pos.0, pos.1, pos.2);
    let (writes, dispensers) = {
        let mut dimensions = live_redstone().lock().unwrap_or_else(|e| e.into_inner());
        let state = dimensions.entry(dim).or_default();
        let mut torches = std::mem::take(&mut state.torches);
        let now = TICK_IDX.load(Ordering::Relaxed);
        let mut world = LiveRedstoneWorld {
            dim,
            reg,
            state,
            now,
            reads: Default::default(),
            writes: Default::default(),
            dispensers: Vec::new(),
        };
        {
            let mut adapter = RedstoneWorldAdapter::new(reg, &mut world, &mut torches);
            adapter.update_neighbour_for_output_signal(origin);
        }
        world.state.torches = torches;
        (world.writes, world.dispensers)
    };
    for (RsPos(x, y, z), state) in writes {
        set_block_if_inner_dim(dim, x, y, z, state, None);
        note_block_update_dim(dim, x, y, z, block_at_dim(dim, x, y, z));
    }
    for RsPos(x, y, z) in dispensers {
        fire_dispenser_dim(dim, x, y, z, block_at_dim(dim, x, y, z));
    }
}

#[cfg(test)]
mod redstone_world_adapter_tests {
    use super::*;

    #[test]
    fn dimension_build_bounds_cover_nether_and_tall_custom_dimensions() {
        assert_eq!(
            redstone_build_height_from_dimension(crate::worldgen::DimId::Nether),
            (0, 256)
        );
    }

    #[test]
    fn live_piston_marks_isolate_same_position_across_dimensions() {
        use crate::worldgen::DimId;
        let pos = RsPos(4, 64, -2);
        let (overworld, nether) = (Some(DimId::Overworld), Some(DimId::Nether));
        let mut marks = LivePistonMarks::default();
        marks.mark(overworld, pos, 10);
        // The Nether view at the same coordinates sees no in-flight marker.
        assert!(marks.moving_at(nether, pos, 10).is_none());
        assert!(marks.moving_at(overworld, pos, 10).is_some());
        // Marking the Nether independently does not disturb the Overworld one.
        marks.mark(nether, pos, 10);
        marks.clear(overworld, pos);
        assert!(marks.moving_at(nether, pos, 10).is_some());
        assert!(marks.moving_at(overworld, pos, 10).is_none());
        // Dimension-agnostic views share the `None` bucket.
        marks.mark(None, pos, 10);
        assert!(marks.moving_at(None, pos, 10).is_some());
        assert!(marks.moving_at(overworld, pos, 10).is_none());
    }
}
