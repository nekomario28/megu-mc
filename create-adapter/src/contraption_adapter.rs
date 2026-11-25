// SPDX-License-Identifier: LGPL-3.0-or-later

//! Create Contraption assembly, rigid motion, passenger carry, actor collision,
//! grid-snapped disassembly, and name-based Save v2 snapshot codec backed by
//! [`mod_offload::contraption::Contraption`].
//!
//! Hardened surface (Lane BEDROCK-CREATE-CONTRAPTION):
//! - [`SuperGlueVolume`] connectivity: adjacent world blocks join a contraption
//!   only when a [`SuperGlueBox`] welds their shared face or a chassis /
//!   slime / honey / anchor-style radial rule grabs them, so unglued floor and
//!   wall blocks are never ripped up.
//! - Fail-closed immovable guard on [`IMMOVABLE_BLOCK_IDS`] during glued
//!   assembly and on every breaking actor.
//! - Bounded linear drivers (Mechanical Piston / Gantry / Rope Pulley) with
//!   exact travel stops and optional bounce, plus Cart Assembler minecart
//!   contraptions that yaw the whole rigid body to the rail heading.
//! - [`ContraptionStorage`] onboard inventory and deterministic
//!   [`step_actors`](LiveContraption::step_actors) effects for drills, saws,
//!   harvesters, ploughs, deployers, and portable storage interfaces with
//!   strict item conservation — drops overflow to the world, never vanish.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use megu_core::world_view::{rotate_block_properties, SubLevelTransform, WorldView};
use mod_adapter_core::blockstates::{format_canonical, BlockRegistry};
use mod_offload::contraption::{Aabb, BlockCluster, ClusterError, Contraption, Pose};

use crate::blockstates::CreateBlockRegistry;
use crate::factory::DimBlockPos;

/// Maximum block count allowed in one assembled contraption (safety bound).
pub const DEFAULT_MAX_CONTRAPTION_BLOCKS: usize = 2048;

/// Vanilla block ids a contraption must never rip into itself, break with an
/// actor, or silently overwrite on disassembly. The guard is fail closed:
/// assembly errors, drills skip the cell, checked disassembly reports the
/// conflict before any edit is committed.
pub const IMMOVABLE_BLOCK_IDS: &[&str] = &[
    "minecraft:bedrock",
    "minecraft:obsidian",
    "minecraft:crying_obsidian",
    "minecraft:respawn_anchor",
    "minecraft:reinforced_deepslate",
    "minecraft:end_portal_frame",
    "minecraft:command_block",
];

/// `true` when `name` is on the fail-closed [`IMMOVABLE_BLOCK_IDS`] list.
pub fn is_immovable_block_id(name: &str) -> bool {
    IMMOVABLE_BLOCK_IDS.contains(&name)
}

/// The six face-adjacent neighbour directions used by both flood fills.
const NEIGHBOR_DIRS: [[i32; 3]; 6] = [
    [1, 0, 0],
    [-1, 0, 0],
    [0, 1, 0],
    [0, -1, 0],
    [0, 0, 1],
    [0, 0, -1],
];

/// Kind of Create contraption motion driver.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ContraptionKind {
    /// Mechanical Bearing rotating about world Y (yaw degrees/tick = RPM * 0.3).
    Bearing { angular_deg_per_tick: f32 },
    /// Mechanical Piston or Gantry translating along a linear velocity vector.
    Linear { velocity_per_tick: [f64; 3] },
    /// Mechanical Piston / Gantry / Rope Pulley with exact travel stops: the
    /// per-axis offset from the assembly anchor is clamped into
    /// `[min_offset, max_offset]`; with `reverse_at_bounds` the velocity on the
    /// clamped axis flips sign (bounce), otherwise it stops cleanly at `0`.
    BoundedLinear {
        velocity_per_tick: [f64; 3],
        min_offset: [f64; 3],
        max_offset: [f64; 3],
        reverse_at_bounds: bool,
    },
    /// Cart Assembler minecart contraption: translates `speed_per_tick` along
    /// `heading_yaw` (degrees about +Y, same convention as the rigid pose —
    /// `0` travels +X, `90` travels -Z) and yaws the body to that heading, so
    /// curved-rail heading changes rotate every local cell and rider coherently.
    Minecart {
        speed_per_tick: f64,
        heading_yaw: f64,
    },
}

impl ContraptionKind {
    /// Fail-closed driver validation: every rate and bound must be finite,
    /// and bounded-linear travel needs `min_offset <= max_offset` per axis.
    pub fn validate(self) -> Result<(), ContraptionError> {
        match self {
            Self::Bearing {
                angular_deg_per_tick,
            } => {
                if !angular_deg_per_tick.is_finite() {
                    return Err(ContraptionError::NonFiniteDriver);
                }
            }
            Self::Linear { velocity_per_tick } => {
                if !velocity_per_tick.iter().all(|v| v.is_finite()) {
                    return Err(ContraptionError::NonFiniteDriver);
                }
            }
            Self::BoundedLinear {
                velocity_per_tick,
                min_offset,
                max_offset,
                ..
            } => {
                if !velocity_per_tick.iter().all(|v| v.is_finite())
                    || !min_offset.iter().all(|v| v.is_finite())
                    || !max_offset.iter().all(|v| v.is_finite())
                {
                    return Err(ContraptionError::NonFiniteDriver);
                }
                if (0..3).any(|axis| min_offset[axis] > max_offset[axis]) {
                    return Err(ContraptionError::InvalidDriverBounds);
                }
            }
            Self::Minecart {
                speed_per_tick,
                heading_yaw,
            } => {
                if !speed_per_tick.is_finite() || !heading_yaw.is_finite() {
                    return Err(ContraptionError::NonFiniteDriver);
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContraptionError {
    EmptyAssembly,
    TooLarge {
        count: usize,
        limit: usize,
    },
    Cluster(ClusterError),
    InvalidPose,
    /// A world coordinate or local offset fell outside `i32` range.
    CoordinateOverflow,
    /// A glued/sticky path reached a block on [`IMMOVABLE_BLOCK_IDS`]; the
    /// assembly refuses instead of ripping bedrock-grade blocks out.
    ImmovableBlock {
        pos: [i32; 3],
    },
    /// A driver parameter was NaN or infinite; the contraption does not move.
    NonFiniteDriver,
    /// Bounded-linear travel had `min_offset > max_offset` on some axis.
    InvalidDriverBounds,
    CorruptSnapshot(String),
}

/// Inclusive 3D Super Glue bounding box in world block coordinates. A glued
/// face between two adjacent cells exists only while one box covers both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SuperGlueBox {
    pub min: [i32; 3],
    pub max: [i32; 3],
}

impl SuperGlueBox {
    /// Normalize two opposite corners so `min <= max` holds on every axis.
    pub fn new(a: [i32; 3], b: [i32; 3]) -> Self {
        Self {
            min: std::array::from_fn(|i| a[i].min(b[i])),
            max: std::array::from_fn(|i| a[i].max(b[i])),
        }
    }

    /// `true` when `pos` lies inside the glued volume (inclusive).
    pub fn contains(&self, pos: [i32; 3]) -> bool {
        (0..3).all(|i| self.min[i] <= pos[i] && pos[i] <= self.max[i])
    }
}

/// The complete glued region applied to a contraption: a set of axis-aligned
/// [`SuperGlueBox`] volumes. Two adjacent world blocks connect only when one
/// box covers both cells — unglued neighbours are never joined.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SuperGlueVolume {
    boxes: Vec<SuperGlueBox>,
}

impl SuperGlueVolume {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_boxes(boxes: Vec<SuperGlueBox>) -> Self {
        Self { boxes }
    }

    pub fn add_box(&mut self, glue_box: SuperGlueBox) {
        self.boxes.push(glue_box);
    }

    pub fn is_empty(&self) -> bool {
        self.boxes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.boxes.len()
    }

    pub fn boxes(&self) -> &[SuperGlueBox] {
        &self.boxes
    }

    /// `true` when any box covers `pos`.
    pub fn contains(&self, pos: [i32; 3]) -> bool {
        self.boxes.iter().any(|b| b.contains(pos))
    }

    /// `true` when one box covers both adjacent cells, welding their shared
    /// face. A box touching only one side does not glue the seam.
    pub fn bridges(&self, a: [i32; 3], b: [i32; 3]) -> bool {
        self.boxes
            .iter()
            .any(|glue_box| glue_box.contains(a) && glue_box.contains(b))
    }
}

/// Bounded multi-slot onboard inventory — chests, barrels and item vaults
/// riding the contraption. `capacity_slots` bounds the number of DISTINCT item
/// ids held at once; stacking into an already-held item is always accepted.
/// Conservation is strict: `insert_item` returns the unaccepted remainder, so
/// `accepted + remainder == requested`, and `extract_item` never removes more
/// than is stored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContraptionStorage {
    capacity_slots: usize,
    slots: BTreeMap<String, u64>,
}

impl ContraptionStorage {
    pub fn new(capacity_slots: usize) -> Self {
        Self {
            capacity_slots,
            slots: BTreeMap::new(),
        }
    }

    pub fn capacity_slots(&self) -> usize {
        self.capacity_slots
    }

    /// Distinct item ids currently held.
    pub fn used_slots(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Sorted `(item_id, count)` view — deterministic for snapshots.
    pub fn slots(&self) -> impl Iterator<Item = (&str, u64)> + '_ {
        self.slots.iter().map(|(id, &count)| (id.as_str(), count))
    }

    /// Total stored count of `item_id` (saturates at `u32::MAX`).
    pub fn total_count_of(&self, item_id: &str) -> u32 {
        self.slots
            .get(item_id)
            .map(|&count| u32::try_from(count).unwrap_or(u32::MAX))
            .unwrap_or(0)
    }

    /// Total stored items across all slots.
    pub fn total_items(&self) -> u64 {
        self.slots.values().sum()
    }

    /// Insert up to `count` of `item_id`; returns the remainder that did not
    /// fit (a new item needs a free slot; an existing stack always accepts).
    pub fn insert_item(&mut self, item_id: &str, count: u32) -> u32 {
        if count == 0 {
            return 0;
        }
        if let Some(held) = self.slots.get_mut(item_id) {
            let next = held.saturating_add(u64::from(count));
            let accepted = next - *held;
            *held = next;
            return count - u32::try_from(accepted).unwrap_or(u32::MAX);
        }
        if self.slots.len() >= self.capacity_slots {
            return count;
        }
        self.slots.insert(item_id.to_string(), u64::from(count));
        0
    }

    /// Extract up to `count` of `item_id`; returns the amount actually
    /// removed (never more than stored). Emptied slots are freed.
    pub fn extract_item(&mut self, item_id: &str, count: u32) -> u32 {
        let Some(held) = self.slots.get_mut(item_id) else {
            return 0;
        };
        let taken = (*held).min(u64::from(count));
        *held -= taken;
        if *held == 0 {
            self.slots.remove(item_id);
        }
        u32::try_from(taken).unwrap_or(u32::MAX)
    }

    /// Extract up to `count` of ANY held item for unfiltered deployers —
    /// deterministic: the lexicographically smallest item id goes first.
    pub fn extract_any(&mut self, count: u32) -> Option<(String, u32)> {
        let item = self.slots.keys().next()?.clone();
        Some((item.clone(), self.extract_item(&item, count)))
    }
}

/// Kind of onboard machine acting on the world cell it faces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContraptionActorKind {
    /// Breaks soft/minable blocks in its swept cell, depositing the block as
    /// an item into [`ContraptionStorage`]; refuses [`IMMOVABLE_BLOCK_IDS`].
    MechanicalDrill,
    /// Breaks blocks like the drill (logs, leaves, foliage).
    MechanicalSaw,
    /// Harvests mature crops: the crop resets to age 0 and stays planted —
    /// the soil is never broken.
    MechanicalHarvester,
    /// Breaks blocks like the drill (tillage/harvest line).
    MechanicalPlough,
    /// Places a block into an empty swept cell, consuming exactly one
    /// matching item from [`ContraptionStorage`] — and ONLY when placement
    /// can succeed. `filter_item: None` deploys the first resolvable held
    /// item (deterministic smallest id).
    MechanicalDeployer { filter_item: Option<String> },
    /// Docked storage handoff marker; performs no world edit by itself.
    PortableStorageInterface,
}

/// Onboard machine mounted on a local contraption cell, sweeping the world
/// cell at `local + facing` (local space) once per actor tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContraptionActor {
    /// Local cell the actor block occupies.
    pub local: [i32; 3],
    /// Local-space direction of the swept cell, e.g. `[1, 0, 0]` = +X.
    pub facing: [i32; 3],
    pub kind: ContraptionActorKind,
}

/// Crop state inside an [`ActorCell`]: harvested when `age >= max_age`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CropCell {
    pub age: u8,
    pub max_age: u8,
    /// Item deposited (or world-dropped on overflow) on mature harvest.
    pub drop_item: String,
    pub drop_count: u32,
}

/// A world cell as seen by a contraption actor during
/// [`LiveContraption::step_actors`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActorCell {
    /// Canonical block id, e.g. `"minecraft:stone"`; `""` means air/empty.
    pub name: String,
    /// Whether the world permits breaking this block. The adapter refuses
    /// [`IMMOVABLE_BLOCK_IDS`] regardless of this flag.
    pub breakable: bool,
    /// Crop state when the cell is a planted crop.
    pub crop: Option<CropCell>,
}

impl ActorCell {
    pub fn air() -> Self {
        Self {
            name: String::new(),
            breakable: false,
            crop: None,
        }
    }

    pub fn block(name: impl Into<String>, breakable: bool) -> Self {
        Self {
            name: name.into(),
            breakable,
            crop: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.name.is_empty()
    }

    pub fn is_immovable(&self) -> bool {
        is_immovable_block_id(&self.name)
    }
}

/// Read-only world surface for one actor tick. Implemented by the server's
/// chunk view (or a test stub); actor effects are returned as an
/// [`ActorTickReport`] and committed by the caller — the world is never
/// mutated through this trait.
pub trait ContraptionWorldQuery {
    /// The block occupying world `pos`; empty cells return [`ActorCell::air`].
    fn cell_at(&self, pos: [i32; 3]) -> ActorCell;

    /// World state id a deployer places for `item_id`, or `None` when the
    /// item cannot be placed as a block.
    fn place_state_for(&self, _item_id: &str) -> Option<u32> {
        None
    }
}

/// A world edit requested by one actor tick; the caller commits these through
/// the same CAS path as [`WorldBlockEdit`] after checking occupancy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActorWorldEdit {
    /// Break the block at `pos` (drill / saw / plough).
    BreakBlock { pos: DimBlockPos },
    /// Harvester reset: the crop stays planted, its age returns to 0.
    ResetCrop { pos: DimBlockPos },
    /// Deployer placed `state_id`, consuming `item_id` from onboard storage.
    PlaceBlock {
        pos: DimBlockPos,
        state_id: u32,
        item_id: String,
    },
}

/// An item ejected into the world because onboard storage had no room —
/// overflow is NEVER silently deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorldItemDrop {
    pub pos: DimBlockPos,
    pub item_id: String,
    pub count: u32,
}

/// Deterministic result of one [`LiveContraption::step_actors`] tick.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActorTickReport {
    pub edits: Vec<ActorWorldEdit>,
    pub drops: Vec<WorldItemDrop>,
}

/// Active assembled contraption with rigid pose, previous pose for passenger
/// carry delta, sparse local block cluster, onboard storage and actors.
pub struct LiveContraption {
    pub id: u32,
    pub dim: i32,
    pub anchor: [i32; 3],
    pub kind: ContraptionKind,
    pub yaw_deg: f32,
    pub pitch_deg: f32,
    pub translation: [f64; 3],
    /// Onboard inventory (capacity 0 until the caller attaches storage).
    pub storage: ContraptionStorage,
    /// Mounted actors in deterministic tick order.
    pub actors: Vec<ContraptionActor>,
    previous_pose: Pose,
    inner: Contraption,
}

/// World-space block edit produced on assembly (removals) or disassembly
/// (placements snapped to integer world grid).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorldBlockEdit {
    pub pos: DimBlockPos,
    /// `None` removes the block (sets air); `Some(state_id)` places it.
    pub state_id: Option<u32>,
}

/// Name-based Save v2 snapshot for a live contraption (survives runtime state
/// ID reordering across mod/addon updates).
#[derive(Clone, Debug, PartialEq)]
pub struct ContraptionSnapshot {
    pub id: u32,
    pub dim: i32,
    pub anchor: [i32; 3],
    pub kind: ContraptionKind,
    pub yaw_deg: f32,
    pub pitch_deg: f32,
    pub translation: [f64; 3],
    /// Local `(x, y, z)` -> canonical blockstate string (or `"vanilla:<id>"`).
    pub cells: Vec<([i32; 3], String)>,
    /// Onboard storage contents; empty for a bare contraption.
    pub storage: ContraptionStorage,
    /// Mounted actors; empty for a bare contraption.
    pub actors: Vec<ContraptionActor>,
}

impl LiveContraption {
    /// Assemble a contraption by 6-connected flood-fill from `seed` over
    /// `attached_blocks` (representing superglued / chassis-bound world blocks).
    /// Returns the assembled [`LiveContraption`] and the list of world-block
    /// removals (`state_id = None`) the server should commit via CAS.
    pub fn assemble_from_world(
        id: u32,
        dim: i32,
        anchor: [i32; 3],
        seed: [i32; 3],
        attached_blocks: &BTreeMap<[i32; 3], u32>,
        kind: ContraptionKind,
        max_blocks: usize,
    ) -> Result<(Self, Vec<WorldBlockEdit>), ContraptionError> {
        kind.validate()?;
        if !attached_blocks.contains_key(&seed) {
            return Err(ContraptionError::EmptyAssembly);
        }

        let mut visited = BTreeSet::new();
        let mut queue = VecDeque::new();
        visited.insert(seed);
        queue.push_back(seed);

        let mut gathered = Vec::new();

        while let Some(pos) = queue.pop_front() {
            if gathered.len() >= max_blocks {
                return Err(ContraptionError::TooLarge {
                    count: gathered.len() + 1,
                    limit: max_blocks,
                });
            }
            gathered.push((pos, attached_blocks[&pos]));

            for d in NEIGHBOR_DIRS {
                // A neighbour past the i32 edge cannot be a map key; skip it
                // instead of panicking on `pos + d` overflow.
                let (Some(nx), Some(ny), Some(nz)) = (
                    pos[0].checked_add(d[0]),
                    pos[1].checked_add(d[1]),
                    pos[2].checked_add(d[2]),
                ) else {
                    continue;
                };
                let n = [nx, ny, nz];
                if attached_blocks.contains_key(&n) && visited.insert(n) {
                    queue.push_back(n);
                }
            }
        }

        Self::finish_assembly(id, dim, anchor, gathered, kind)
    }

    /// Assemble by flood-fill from `seed` over every candidate in
    /// `world_blocks`, gated by real connectivity: two adjacent cells join
    /// only when `glue` welds their shared face or either side is a
    /// chassis/slime/honey/anchor-style radial grabber
    /// (`is_radial_sticky(state)`). Unglued plain neighbours — floor, walls —
    /// are never pulled in. Any reached cell on [`IMMOVABLE_BLOCK_IDS`]
    /// (per `is_immovable`) fails the whole assembly closed.
    #[allow(clippy::too_many_arguments)]
    pub fn assemble_with_glue(
        id: u32,
        dim: i32,
        anchor: [i32; 3],
        seed: [i32; 3],
        world_blocks: &BTreeMap<[i32; 3], u32>,
        glue: &SuperGlueVolume,
        is_radial_sticky: impl Fn(u32) -> bool,
        is_immovable: impl Fn(u32) -> bool,
        kind: ContraptionKind,
        max_blocks: usize,
    ) -> Result<(Self, Vec<WorldBlockEdit>), ContraptionError> {
        kind.validate()?;
        if !world_blocks.contains_key(&seed) {
            return Err(ContraptionError::EmptyAssembly);
        }

        let mut visited = BTreeSet::new();
        let mut queue = VecDeque::new();
        visited.insert(seed);
        queue.push_back(seed);

        let mut gathered = Vec::new();

        while let Some(pos) = queue.pop_front() {
            if gathered.len() >= max_blocks {
                return Err(ContraptionError::TooLarge {
                    count: gathered.len() + 1,
                    limit: max_blocks,
                });
            }
            let state_id = world_blocks[&pos];
            if is_immovable(state_id) {
                return Err(ContraptionError::ImmovableBlock { pos });
            }
            gathered.push((pos, state_id));

            for d in NEIGHBOR_DIRS {
                let (Some(nx), Some(ny), Some(nz)) = (
                    pos[0].checked_add(d[0]),
                    pos[1].checked_add(d[1]),
                    pos[2].checked_add(d[2]),
                ) else {
                    continue;
                };
                let n = [nx, ny, nz];
                if visited.contains(&n) {
                    continue;
                }
                let Some(&n_state) = world_blocks.get(&n) else {
                    continue;
                };
                // Join only through a glue weld or a radial grabber on either
                // side of the seam — never through plain adjacency.
                if !(glue.bridges(pos, n)
                    || is_radial_sticky(state_id)
                    || is_radial_sticky(n_state))
                {
                    continue;
                }
                visited.insert(n);
                queue.push_back(n);
            }
        }

        Self::finish_assembly(id, dim, anchor, gathered, kind)
    }

    /// Shared tail for both assembly paths: world positions -> local cells,
    /// cluster, removal edits and identity pose at `anchor`.
    fn finish_assembly(
        id: u32,
        dim: i32,
        anchor: [i32; 3],
        gathered: Vec<([i32; 3], u32)>,
        kind: ContraptionKind,
    ) -> Result<(Self, Vec<WorldBlockEdit>), ContraptionError> {
        let mut local_cells = Vec::with_capacity(gathered.len());
        let mut world_removals = Vec::with_capacity(gathered.len());
        for (pos, state_id) in gathered {
            let mut local = [0_i32; 3];
            for axis in 0..3 {
                local[axis] = pos[axis]
                    .checked_sub(anchor[axis])
                    .ok_or(ContraptionError::CoordinateOverflow)?;
            }
            local_cells.push((local, state_id));
            world_removals.push(WorldBlockEdit {
                pos: DimBlockPos::new(dim, pos[0], pos[1], pos[2]),
                state_id: None,
            });
        }
        let cluster = BlockCluster::new(local_cells).map_err(ContraptionError::Cluster)?;
        let translation = [
            f64::from(anchor[0]),
            f64::from(anchor[1]),
            f64::from(anchor[2]),
        ];
        let pose =
            Pose::from_yaw_pitch(translation, 0.0, 0.0).ok_or(ContraptionError::InvalidPose)?;
        let inner = Contraption::new(cluster, pose);
        Ok((
            Self {
                id,
                dim,
                anchor,
                kind,
                yaw_deg: 0.0,
                pitch_deg: 0.0,
                translation,
                storage: ContraptionStorage::new(0),
                actors: Vec::new(),
                previous_pose: pose,
                inner,
            },
            world_removals,
        ))
    }

    pub fn block_count(&self) -> usize {
        self.inner.cluster().blocks().len()
    }

    pub fn world_aabb(&self) -> Option<Aabb> {
        self.inner.world_aabb()
    }

    pub fn pose(&self) -> Pose {
        self.inner.pose()
    }

    /// Advance the contraption by one game tick according to its motion driver.
    /// Returns `true` if the rigid pose changed.
    pub fn step_tick(&mut self) -> Result<bool, ContraptionError> {
        // Validate before touching `previous_pose`: a corrupt driver leaves
        // the contraption exactly where it was (fail closed).
        self.kind.validate()?;
        self.previous_pose = self.inner.pose();
        match &mut self.kind {
            ContraptionKind::Bearing {
                angular_deg_per_tick,
            } => {
                self.yaw_deg = (self.yaw_deg + *angular_deg_per_tick).rem_euclid(360.0);
            }
            ContraptionKind::Linear { velocity_per_tick } => {
                for (dst, vel) in self.translation.iter_mut().zip(velocity_per_tick.iter()) {
                    *dst += *vel;
                }
            }
            ContraptionKind::BoundedLinear {
                velocity_per_tick,
                min_offset,
                max_offset,
                reverse_at_bounds,
            } => {
                let bounce = *reverse_at_bounds;
                for axis in 0..3 {
                    let origin = f64::from(self.anchor[axis]);
                    let next = self.translation[axis] - origin + velocity_per_tick[axis];
                    if next > max_offset[axis] {
                        self.translation[axis] = origin + max_offset[axis];
                        velocity_per_tick[axis] = if bounce {
                            -velocity_per_tick[axis]
                        } else {
                            0.0
                        };
                    } else if next < min_offset[axis] {
                        self.translation[axis] = origin + min_offset[axis];
                        velocity_per_tick[axis] = if bounce {
                            -velocity_per_tick[axis]
                        } else {
                            0.0
                        };
                    } else {
                        self.translation[axis] = origin + next;
                    }
                }
            }
            ContraptionKind::Minecart {
                speed_per_tick,
                heading_yaw,
            } => {
                self.yaw_deg = heading_yaw.rem_euclid(360.0) as f32;
                let (sin, cos) = heading_yaw.to_radians().sin_cos();
                // Quarter-turn headings leave ~6e-17 trig residue; snapping it
                // keeps rail turns at exactly zero drift.
                let snap = |v: f64| if v.abs() < 1.0e-15 { 0.0 } else { v };
                self.translation[0] += *speed_per_tick * snap(cos);
                self.translation[2] += *speed_per_tick * -snap(sin);
            }
        }
        let next = Pose::from_yaw_pitch(self.translation, self.yaw_deg, self.pitch_deg)
            .ok_or(ContraptionError::InvalidPose)?;
        Ok(self.inner.set_pose(next).is_some())
    }

    /// Exact oriented-voxel collision check against an actor world AABB.
    pub fn collides(&self, actor_bounds: Aabb) -> bool {
        self.inner.intersects(actor_bounds)
    }

    /// Compute the rigid displacement of a carried passenger anchor from the
    /// previous tick's pose to the current tick's pose.
    pub fn passenger_delta(&self, previous_anchor: [f64; 3]) -> Option<[f64; 3]> {
        self.inner
            .pose()
            .carried_delta(self.previous_pose, previous_anchor)
    }

    /// Disassemble the contraption by snapping its current rigid rotation to
    /// the nearest 90° increment and translation to integer world coordinates.
    /// Blockstate ids pass through unrotated — callers with a
    /// [`BlockRegistry`] use [`Self::disassemble_to_world_with_rotator`] so
    /// directional properties (`facing`, `axis`, rail `shape`, cardinal
    /// connection keys…) rotate together with the cell coordinates.
    pub fn disassemble_to_world(&self) -> Vec<WorldBlockEdit> {
        self.disassemble_to_world_with_rotator(|state_id, _turns| state_id)
    }

    /// [`disassemble_to_world`](Self::disassemble_to_world) with a blockstate
    /// rotator: `rotate_state(state_id, rot)` maps each cell's runtime state
    /// id to its `rot`-quarter-turns-rotated sibling (`rot` in `0..=3`, the
    /// same convention the `(x, z) -> (z, -x)` coordinate map and
    /// [`megu_core::world_view::rotate_block_properties`] share). Compose it
    /// from [`registry_state_rotator`] when a [`BlockRegistry`] is at hand.
    pub fn disassemble_to_world_with_rotator(
        &self,
        rotate_state: impl Fn(u32, i32) -> u32,
    ) -> Vec<WorldBlockEdit> {
        let quarter_turns = (self.yaw_deg / 90.0).round() as i32;
        let rot = quarter_turns.rem_euclid(4);
        let base_x = self.translation[0].round() as i32;
        let base_y = self.translation[1].round() as i32;
        let base_z = self.translation[2].round() as i32;

        self.inner
            .cluster()
            .blocks()
            .map(|([lx, ly, lz], state_id)| {
                // Right-handed rotation about +Y in 90-degree increments matches
                // Pose::from_yaw_pitch's matrix [[cos, 0, sin], [0, 1, 0], [-sin, 0, cos]].
                let (rx, rz) = match rot {
                    0 => (lx, lz),
                    1 => (lz, -lx),
                    2 => (-lx, -lz),
                    3 => (-lz, lx),
                    _ => unreachable!(),
                };
                WorldBlockEdit {
                    // `base_*` saturates at the i32 edge after `as i32` on a
                    // huge translation; clamp the final sum too so a long
                    // Linear run cannot panic on `base + r` overflow.
                    pos: DimBlockPos::new(
                        self.dim,
                        base_x.saturating_add(rx),
                        base_y.saturating_add(ly),
                        base_z.saturating_add(rz),
                    ),
                    state_id: Some(rotate_state(state_id, rot)),
                }
            })
            .collect()
    }

    /// Checked variant of [`disassemble_to_world`](Self::disassemble_to_world):
    /// queries `is_occupied` for every target cell and fails closed with the
    /// exact conflicting cells — e.g. regenerated bedrock — instead of
    /// overwriting them. `Ok(edits)` is identical to the unchecked result.
    pub fn disassemble_to_world_checked(
        &self,
        is_occupied: impl Fn(DimBlockPos) -> bool,
    ) -> Result<Vec<WorldBlockEdit>, Vec<DimBlockPos>> {
        self.disassemble_to_world_checked_with_rotator(is_occupied, |state_id, _turns| state_id)
    }

    /// [`disassemble_to_world_checked`](Self::disassemble_to_world_checked)
    /// with a blockstate rotator — the conflict list reports the rotated
    /// target cells exactly as
    /// [`disassemble_to_world_with_rotator`](Self::disassemble_to_world_with_rotator)
    /// would emit them.
    pub fn disassemble_to_world_checked_with_rotator(
        &self,
        is_occupied: impl Fn(DimBlockPos) -> bool,
        rotate_state: impl Fn(u32, i32) -> u32,
    ) -> Result<Vec<WorldBlockEdit>, Vec<DimBlockPos>> {
        let edits = self.disassemble_to_world_with_rotator(rotate_state);
        let conflicts: Vec<DimBlockPos> = edits
            .iter()
            .filter(|edit| is_occupied(edit.pos))
            .map(|edit| edit.pos)
            .collect();
        if conflicts.is_empty() {
            Ok(edits)
        } else {
            Err(conflicts)
        }
    }

    /// Deterministic onboard-actor tick: every mounted actor acts on its
    /// swept world cell in mount order. Effects are reported, not applied —
    /// the caller commits them. Conservation is strict: mined/harvested items
    /// go to [`ContraptionStorage`], overflow becomes a [`WorldItemDrop`], and
    /// a deployer consumes an item only when placement can succeed.
    pub fn step_actors(&mut self, world_query: &dyn ContraptionWorldQuery) -> ActorTickReport {
        let mut report = ActorTickReport::default();
        let Self {
            inner,
            dim,
            storage,
            actors,
            ..
        } = self;
        let dim = *dim;
        for actor in actors.iter() {
            let cell_pos = actor_target_cell(inner, actor);
            let pos = DimBlockPos::new(dim, cell_pos[0], cell_pos[1], cell_pos[2]);
            match &actor.kind {
                ContraptionActorKind::MechanicalDrill
                | ContraptionActorKind::MechanicalSaw
                | ContraptionActorKind::MechanicalPlough => {
                    let cell = world_query.cell_at(cell_pos);
                    if cell.is_empty() || cell.is_immovable() || !cell.breakable {
                        continue;
                    }
                    report.edits.push(ActorWorldEdit::BreakBlock { pos });
                    let remainder = storage.insert_item(&cell.name, 1);
                    if remainder > 0 {
                        report.drops.push(WorldItemDrop {
                            pos,
                            item_id: cell.name,
                            count: remainder,
                        });
                    }
                }
                ContraptionActorKind::MechanicalHarvester => {
                    let cell = world_query.cell_at(cell_pos);
                    if cell.is_immovable() {
                        continue;
                    }
                    if let Some(crop) = &cell.crop {
                        if crop.age >= crop.max_age {
                            // Reset to age 0; the plant and soil stay in place.
                            report.edits.push(ActorWorldEdit::ResetCrop { pos });
                            let remainder = storage.insert_item(&crop.drop_item, crop.drop_count);
                            if remainder > 0 {
                                report.drops.push(WorldItemDrop {
                                    pos,
                                    item_id: crop.drop_item.clone(),
                                    count: remainder,
                                });
                            }
                        }
                    }
                }
                ContraptionActorKind::MechanicalDeployer { filter_item } => {
                    let cell = world_query.cell_at(cell_pos);
                    if !cell.is_empty() {
                        continue;
                    }
                    match filter_item {
                        Some(item) => {
                            let Some(state_id) = world_query.place_state_for(item) else {
                                continue;
                            };
                            // Consume ONLY on successful placement — zero loss.
                            if storage.extract_item(item, 1) == 1 {
                                report.edits.push(ActorWorldEdit::PlaceBlock {
                                    pos,
                                    state_id,
                                    item_id: item.clone(),
                                });
                            }
                        }
                        None => {
                            let candidates: Vec<String> =
                                storage.slots().map(|(id, _)| id.to_string()).collect();
                            for item in candidates {
                                let Some(state_id) = world_query.place_state_for(&item) else {
                                    continue;
                                };
                                if storage.extract_item(&item, 1) == 1 {
                                    report.edits.push(ActorWorldEdit::PlaceBlock {
                                        pos,
                                        state_id,
                                        item_id: item,
                                    });
                                    break;
                                }
                            }
                        }
                    }
                }
                ContraptionActorKind::PortableStorageInterface => {}
            }
        }
        report
    }

    /// Capture a Save v2 name-based snapshot using `registry` for Create states
    /// and `"vanilla:<id>"` for vanilla states.
    pub fn snapshot(&self, registry: &CreateBlockRegistry) -> ContraptionSnapshot {
        let cells = self
            .inner
            .cluster()
            .blocks()
            .map(|(local, state_id)| {
                let encoded = registry
                    .encode_canonical(state_id)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("vanilla:{state_id}"));
                (local, encoded)
            })
            .collect();
        ContraptionSnapshot {
            id: self.id,
            dim: self.dim,
            anchor: self.anchor,
            kind: self.kind,
            yaw_deg: self.yaw_deg,
            pitch_deg: self.pitch_deg,
            translation: self.translation,
            cells,
            storage: self.storage.clone(),
            actors: self.actors.clone(),
        }
    }

    /// Restore a contraption from a Save v2 snapshot against a (possibly
    /// reordered) [`CreateBlockRegistry`].
    pub fn restore(
        snapshot: &ContraptionSnapshot,
        registry: &CreateBlockRegistry,
    ) -> Result<Self, ContraptionError> {
        snapshot.kind.validate()?;
        let mut local_cells = Vec::with_capacity(snapshot.cells.len());
        for (local, name) in &snapshot.cells {
            let state_id = if let Some(raw) = name.strip_prefix("vanilla:") {
                raw.parse::<u32>()
                    .map_err(|_| ContraptionError::CorruptSnapshot(name.clone()))?
            } else {
                registry
                    .decode_canonical(name)
                    .ok_or_else(|| ContraptionError::CorruptSnapshot(name.clone()))?
            };
            local_cells.push((*local, state_id));
        }
        let cluster = BlockCluster::new(local_cells).map_err(ContraptionError::Cluster)?;
        let pose = Pose::from_yaw_pitch(snapshot.translation, snapshot.yaw_deg, snapshot.pitch_deg)
            .ok_or(ContraptionError::InvalidPose)?;
        let inner = Contraption::new(cluster, pose);
        Ok(Self {
            id: snapshot.id,
            dim: snapshot.dim,
            anchor: snapshot.anchor,
            kind: snapshot.kind,
            yaw_deg: snapshot.yaw_deg,
            pitch_deg: snapshot.pitch_deg,
            translation: snapshot.translation,
            storage: snapshot.storage.clone(),
            actors: snapshot.actors.clone(),
            previous_pose: pose,
            inner,
        })
    }
}

/// Compose a `rotate_state(state_id, rot)` closure over a [`BlockRegistry`]
/// for [`LiveContraption::disassemble_to_world_with_rotator`]: resolves the
/// runtime state id to its canonical properties, runs
/// [`megu_core::world_view::rotate_block_properties`] (every directional
/// property — `facing`, `axis`, `rotation`, `orientation`, rail `shape`,
/// cardinal connection keys), and resolves the rotated canonical form back
/// to a runtime id. Unknown states and `rot == 0` pass through unchanged so
/// propertyless blocks and unresolved registries degrade gracefully.
pub fn registry_state_rotator<'a, R>(
    registry: &'a BlockRegistry<R>,
) -> impl Fn(u32, i32) -> u32 + 'a {
    move |state_id, turns| {
        let turns = turns.rem_euclid(4);
        if turns == 0 {
            return state_id;
        }
        let Some(entry) = registry.state(state_id) else {
            return state_id;
        };
        if entry.properties.is_empty() {
            return state_id;
        }
        let props: Vec<(&str, &str)> = entry
            .properties
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let rotated = rotate_block_properties(&props, turns);
        let canonical = format_canonical(&entry.block_id, &rotated);
        registry.decode_canonical(&canonical).unwrap_or(state_id)
    }
}

/// Canonical [`WorldView`] for a live contraption sub-level: local block
/// cells read/write through the paletted [`BlockCluster`], and
/// [`Self::world_transform`] reports the rigid [`Pose`] (pivot at the local
/// origin — `Pose` translation already refers to it). Writes rebuild the
/// cluster via [`Contraption::replace_cluster`] so palette invariants stay
/// intact; an invalid rebuild is refused (`false`).
impl WorldView for LiveContraption {
    fn block_state_at(&self, pos: [i32; 3]) -> Option<u32> {
        self.inner.cluster().state_at(pos)
    }

    fn set_block_state(&mut self, pos: [i32; 3], state: u32) -> bool {
        let mut cells: Vec<([i32; 3], u32)> = self.inner.cluster().blocks().collect();
        match cells.iter_mut().find(|(p, _)| *p == pos) {
            Some(slot) => slot.1 = state,
            None => cells.push((pos, state)),
        }
        match BlockCluster::new(cells) {
            Ok(cluster) => {
                self.inner.replace_cluster(cluster);
                true
            }
            Err(_) => false,
        }
    }

    fn world_transform(&self) -> SubLevelTransform {
        let pose = self.inner.pose();
        SubLevelTransform {
            rotation: pose.rotation(),
            translation: pose.translation(),
            pivot: [0.0; 3],
        }
    }
}

/// The world cell swept by `actor` this tick: `local + facing` rotated by the
/// rigid pose and snapped to the integer grid.
fn actor_target_cell(inner: &Contraption, actor: &ContraptionActor) -> [i32; 3] {
    let pose = inner.pose();
    let rot = pose.rotation();
    let t = pose.translation();
    let center = [
        f64::from(actor.local[0].saturating_add(actor.facing[0])) + 0.5,
        f64::from(actor.local[1].saturating_add(actor.facing[1])) + 0.5,
        f64::from(actor.local[2].saturating_add(actor.facing[2])) + 0.5,
    ];
    // +epsilon absorbs the ~1e-16 residue exact quarter-turns leave in
    // sin/cos so the snapped cell never flips on a boundary.
    std::array::from_fn(|i| {
        (rot[i][0] * center[0] + rot[i][1] * center[1] + rot[i][2] * center[2] + t[i] + 1.0e-9)
            .floor() as i32
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockstates::{core_create_registry, DEFAULT_BASE_STATE_ID};

    #[test]
    fn bearing_contraption_assemble_rotate_carry_passenger_and_disassemble() {
        let reg = core_create_registry(DEFAULT_BASE_STATE_ID);
        let shaft = reg.default_state("create:shaft").unwrap();
        let mut world = BTreeMap::new();
        // 3-block horizontal arm at anchor (10, 64, 10) extending along +X.
        world.insert([10, 64, 10], shaft);
        world.insert([11, 64, 10], shaft);
        world.insert([12, 64, 10], shaft);
        // Unattached distant block must not be pulled in.
        world.insert([20, 64, 20], 1);

        let (mut contraption, removals) = LiveContraption::assemble_from_world(
            7,
            0,
            [10, 64, 10],
            [10, 64, 10],
            &world,
            ContraptionKind::Bearing {
                angular_deg_per_tick: 15.0,
            },
            DEFAULT_MAX_CONTRAPTION_BLOCKS,
        )
        .unwrap();
        assert_eq!(contraption.block_count(), 3);
        assert_eq!(removals.len(), 3);

        // Passenger standing on block (12, 64, 10) center = (12.5, 65.0, 10.5).
        let mut passenger = [12.5, 65.0, 10.5];
        for _ in 0..6 {
            assert!(contraption.step_tick().unwrap());
            let delta = contraption.passenger_delta(passenger).unwrap();
            for i in 0..3 {
                passenger[i] += delta[i];
            }
        }
        // After 6 * 15 = 90 degrees about Y around (10, 64, 10),
        // local (+2.5, +1.0, +0.5) maps to (+0.5, +1.0, -2.5) -> world (10.5, 65.0, 7.5).
        assert!((passenger[0] - 10.5).abs() < 1e-6);
        assert!((passenger[1] - 65.0).abs() < 1e-6);
        assert!((passenger[2] - 7.5).abs() < 1e-6);

        // Collision hits rotated arm at (10, 64, 8), misses old (12, 64, 10).
        let hit_box = Aabb::new([10.2, 64.2, 8.2], [10.8, 64.8, 8.8]).unwrap();
        let miss_box = Aabb::new([12.2, 64.2, 10.2], [12.8, 64.8, 10.8]).unwrap();
        assert!(contraption.collides(hit_box));
        assert!(!contraption.collides(miss_box));

        // Disassemble snaps cleanly to (10,64,10), (10,64,9), (10,64,8).
        let placed = contraption.disassemble_to_world();
        let coords: BTreeSet<(i32, i32, i32)> =
            placed.iter().map(|e| (e.pos.x, e.pos.y, e.pos.z)).collect();
        assert!(coords.contains(&(10, 64, 10)));
        assert!(coords.contains(&(10, 64, 9)));
        assert!(coords.contains(&(10, 64, 8)));

        // Save v2 snapshot round-trip preserves pose and blocks cleanly.
        let snap = contraption.snapshot(&reg);
        let restored = LiveContraption::restore(&snap, &reg).unwrap();
        assert_eq!(restored.block_count(), 3);
        assert!((restored.yaw_deg - 90.0).abs() < 1e-6);
    }

    #[test]
    fn assembly_at_i32_edges_fails_closed_or_skips() {
        let reg = core_create_registry(DEFAULT_BASE_STATE_ID);
        let shaft = reg.default_state("create:shaft").unwrap();

        // Seed at the +X edge: the +X neighbour overflows i32 and must be
        // skipped, not panic; local coords still resolve.
        let mut world = BTreeMap::new();
        world.insert([i32::MAX, 64, 10], shaft);
        world.insert([i32::MAX - 1, 64, 10], shaft);
        let (c, _) = LiveContraption::assemble_from_world(
            1,
            0,
            [i32::MAX, 64, 10],
            [i32::MAX, 64, 10],
            &world,
            ContraptionKind::Linear {
                velocity_per_tick: [0.0; 3],
            },
            DEFAULT_MAX_CONTRAPTION_BLOCKS,
        )
        .unwrap();
        assert_eq!(c.block_count(), 2);

        // Anchor on the opposite edge makes `pos - anchor` overflow i32;
        // the assembly must report CoordinateOverflow, not panic.
        let mut world = BTreeMap::new();
        world.insert([i32::MAX, 64, 10], shaft);
        let err = match LiveContraption::assemble_from_world(
            2,
            0,
            [i32::MIN, 64, 10],
            [i32::MAX, 64, 10],
            &world,
            ContraptionKind::Linear {
                velocity_per_tick: [0.0; 3],
            },
            DEFAULT_MAX_CONTRAPTION_BLOCKS,
        ) {
            Err(e) => e,
            Ok(_) => panic!("expected CoordinateOverflow"),
        };
        assert_eq!(err, ContraptionError::CoordinateOverflow);
    }

    #[test]
    fn disassemble_saturates_after_huge_translation() {
        let reg = core_create_registry(DEFAULT_BASE_STATE_ID);
        let shaft = reg.default_state("create:shaft").unwrap();
        let mut world = BTreeMap::new();
        world.insert([10, 64, 10], shaft);
        world.insert([11, 64, 10], shaft);
        let (mut c, _) = LiveContraption::assemble_from_world(
            3,
            0,
            [10, 64, 10],
            [10, 64, 10],
            &world,
            ContraptionKind::Linear {
                velocity_per_tick: [1.0e300; 3],
            },
            DEFAULT_MAX_CONTRAPTION_BLOCKS,
        )
        .unwrap();
        // Drive the translation far past the i32 range, then disassemble:
        // `round() as i32` saturates and the final `base + r` must not panic.
        c.translation = [1.0e300, -1.0e300, 1.0e300];
        let edits = c.disassemble_to_world();
        assert_eq!(edits.len(), 2);
        // Saturated to the i32 edges rather than wrapping or panicking.
        assert!(edits.iter().all(|e| e.pos.x == i32::MAX));
        assert!(edits.iter().all(|e| e.pos.y == i32::MIN));
    }

    // -- P0-E: blockstate rotation on disassembly -----------------------

    /// Tiny registry with real directional properties so the rotator has
    /// something to resolve against (`R = ()`: no gameplay role needed).
    fn directional_test_registry() -> BlockRegistry<()> {
        use mod_adapter_core::blockstates::{BlockDef, PropertyDef};
        BlockRegistry::build(
            DEFAULT_BASE_STATE_ID,
            vec![
                BlockDef::<()> {
                    id: "test:furnace".into(),
                    properties: vec![PropertyDef::new(
                        "facing",
                        &["north", "east", "south", "west", "up", "down"],
                    )],
                    load_factor: 0.0,
                    capacity_factor: 0.0,
                    role: (),
                },
                BlockDef::<()> {
                    id: "test:log".into(),
                    properties: vec![PropertyDef::new("axis", &["x", "y", "z"])],
                    load_factor: 0.0,
                    capacity_factor: 0.0,
                    role: (),
                },
                BlockDef::<()> {
                    id: "test:rail".into(),
                    properties: vec![PropertyDef::new(
                        "shape",
                        &[
                            "north_south",
                            "east_west",
                            "ascending_east",
                            "ascending_north",
                            "ascending_west",
                            "ascending_south",
                            "south_east",
                            "north_east",
                            "north_west",
                            "south_west",
                        ],
                    )],
                    load_factor: 0.0,
                    capacity_factor: 0.0,
                    role: (),
                },
                BlockDef::<()> {
                    id: "test:pipe".into(),
                    properties: vec![
                        PropertyDef::new("north", &["none", "side"]),
                        PropertyDef::new("east", &["none", "side"]),
                        PropertyDef::new("south", &["none", "side"]),
                        PropertyDef::new("west", &["none", "side"]),
                    ],
                    load_factor: 0.0,
                    capacity_factor: 0.0,
                    role: (),
                },
                BlockDef::<()> {
                    id: "test:plain".into(),
                    properties: vec![],
                    load_factor: 0.0,
                    capacity_factor: 0.0,
                    role: (),
                },
            ],
        )
        .unwrap()
    }

    fn gather_by_pos(edits: &[WorldBlockEdit]) -> BTreeMap<(i32, i32, i32), Option<u32>> {
        edits
            .iter()
            .map(|e| ((e.pos.x, e.pos.y, e.pos.z), e.state_id))
            .collect()
    }

    #[test]
    fn disassembly_90_deg_rotates_cells_and_directional_states_together() {
        let reg = directional_test_registry();
        let east = reg.decode_canonical("test:furnace[facing=east]").unwrap();
        let x_axis = reg.decode_canonical("test:log[axis=x]").unwrap();
        let ew_rail = reg.decode_canonical("test:rail[shape=east_west]").unwrap();
        let asc_east = reg.decode_canonical("test:rail[shape=ascending_east]").unwrap();
        let pipe_n = reg
            .decode_canonical("test:pipe[north=side,east=none,south=none,west=none]")
            .unwrap();
        let plain = reg.decode_canonical("test:plain").unwrap();

        let mut world = BTreeMap::new();
        world.insert([10, 64, 10], east);
        world.insert([11, 64, 10], x_axis);
        world.insert([12, 64, 10], ew_rail);
        world.insert([13, 64, 10], asc_east);
        world.insert([14, 64, 10], pipe_n);
        world.insert([15, 64, 10], plain);
        let (mut c, _) = LiveContraption::assemble_from_world(
            1,
            0,
            [10, 64, 10],
            [10, 64, 10],
            &world,
            ContraptionKind::Linear {
                velocity_per_tick: [0.0; 3],
            },
            DEFAULT_MAX_CONTRAPTION_BLOCKS,
        )
        .unwrap();

        // 90° yaw: local +x lands on world -z; facing east -> north,
        // axis x -> z, east_west -> north_south, ascending_east ->
        // ascending_north, the pipe's north=side leg rotates to west=side.
        c.yaw_deg = 90.0;
        let edits = c.disassemble_to_world_with_rotator(registry_state_rotator(&reg));
        let at = gather_by_pos(&edits);
        assert_eq!(edits.len(), 6);
        let state = |canonical: &str| Some(reg.decode_canonical(canonical).unwrap());
        assert_eq!(
            at[&(10, 64, 10)],
            state("test:furnace[facing=north]")
        );
        assert_eq!(at[&(10, 64, 9)], state("test:log[axis=z]"));
        assert_eq!(at[&(10, 64, 8)], state("test:rail[shape=north_south]"));
        assert_eq!(
            at[&(10, 64, 7)],
            state("test:rail[shape=ascending_north]")
        );
        assert_eq!(
            at[&(10, 64, 6)],
            state("test:pipe[north=none,east=none,south=none,west=side]")
        );
        // Propertyless blocks pass through untouched.
        assert_eq!(at[&(10, 64, 5)], Some(plain));
    }

    #[test]
    fn disassembly_180_and_270_rotate_states_consistently_with_coordinates() {
        let reg = directional_test_registry();
        let east = reg.decode_canonical("test:furnace[facing=east]").unwrap();
        let x_axis = reg.decode_canonical("test:log[axis=x]").unwrap();
        let asc_east = reg.decode_canonical("test:rail[shape=ascending_east]").unwrap();
        let mut world = BTreeMap::new();
        world.insert([10, 64, 10], east);
        world.insert([11, 64, 10], x_axis);
        world.insert([12, 64, 10], asc_east);
        let build = |yaw: f32| {
            let (mut c, _) = LiveContraption::assemble_from_world(
                2,
                0,
                [10, 64, 10],
                [10, 64, 10],
                &world,
                ContraptionKind::Linear {
                    velocity_per_tick: [0.0; 3],
                },
                DEFAULT_MAX_CONTRAPTION_BLOCKS,
            )
            .unwrap();
            c.yaw_deg = yaw;
            gather_by_pos(&c.disassemble_to_world_with_rotator(registry_state_rotator(&reg)))
        };
        let state = |canonical: &str| Some(reg.decode_canonical(canonical).unwrap());

        // 180°: (x,z) -> (-x,-z); facing east -> west, axis x -> x (even
        // turn keeps the axis), ascending_east -> ascending_west.
        let at = build(180.0);
        assert_eq!(at[&(10, 64, 10)], state("test:furnace[facing=west]"));
        assert_eq!(at[&(9, 64, 10)], state("test:log[axis=x]"));
        assert_eq!(at[&(8, 64, 10)], state("test:rail[shape=ascending_west]"));

        // 270°: (x,z) -> (-z,x); facing east -> south — the same
        // right-handed convention the coordinate map uses: east(+x) -> +z
        // which is `south`; ascending_east -> ascending_south.
        let at = build(270.0);
        assert_eq!(at[&(10, 64, 10)], state("test:furnace[facing=south]"));
        assert_eq!(at[&(10, 64, 11)], state("test:log[axis=z]"));
        assert_eq!(at[&(10, 64, 12)], state("test:rail[shape=ascending_south]"));
    }

    #[test]
    fn disassembly_without_rotator_preserves_states_and_checked_reports_conflicts() {
        let reg = directional_test_registry();
        let east = reg.decode_canonical("test:furnace[facing=east]").unwrap();
        let mut world = BTreeMap::new();
        world.insert([10, 64, 10], east);
        world.insert([11, 64, 10], east);
        let (mut c, _) = LiveContraption::assemble_from_world(
            3,
            0,
            [10, 64, 10],
            [10, 64, 10],
            &world,
            ContraptionKind::Linear {
                velocity_per_tick: [0.0; 3],
            },
            DEFAULT_MAX_CONTRAPTION_BLOCKS,
        )
        .unwrap();
        c.yaw_deg = 90.0;
        // Legacy entry point: coordinates rotate, state ids pass through.
        let at = gather_by_pos(&c.disassemble_to_world());
        assert_eq!(at[&(10, 64, 10)], Some(east));
        assert_eq!(at[&(10, 64, 9)], Some(east));

        // The checked rotator variant reports conflicts at the *rotated*
        // target cells.
        let conflicts = c
            .disassemble_to_world_checked_with_rotator(
                |p| p.x == 10 && p.z == 9,
                registry_state_rotator(&reg),
            )
            .unwrap_err();
        assert_eq!(conflicts.len(), 1);
        assert_eq!((conflicts[0].x, conflicts[0].z), (10, 9));
    }

    #[test]
    fn registry_state_rotator_passes_through_unknown_and_zero_turn_states() {
        let reg = directional_test_registry();
        let rot = registry_state_rotator(&reg);
        let east = reg.decode_canonical("test:furnace[facing=east]").unwrap();
        assert_eq!(rot(east, 0), east);
        assert_eq!(rot(east, 4), east, "a full turn is the identity");
        assert_eq!(rot(east, -3), rot(east, 1), "turns are mod-4 Euclidean");
        assert_eq!(rot(u32::MAX, 1), u32::MAX, "unresolved states pass through");
        // Invalid combinations (e.g. a canonical that resolves to no
        // registered state) degrade to the input id.
        let plain = reg.decode_canonical("test:plain").unwrap();
        assert_eq!(rot(plain, 1), plain);
    }

    // -- Common foundation: WorldView on the live contraption ------------

    #[test]
    fn world_view_reads_writes_and_reports_the_rigid_pose() {
        let reg = core_create_registry(DEFAULT_BASE_STATE_ID);
        let shaft = reg.default_state("create:shaft").unwrap();
        let mut world = BTreeMap::new();
        world.insert([10, 64, 10], shaft);
        let (mut c, _) = LiveContraption::assemble_from_world(
            4,
            0,
            [10, 64, 10],
            [10, 64, 10],
            &world,
            ContraptionKind::Linear {
                velocity_per_tick: [0.0; 3],
            },
            DEFAULT_MAX_CONTRAPTION_BLOCKS,
        )
        .unwrap();
        assert_eq!(WorldView::block_state_at(&c, [0, 0, 0]), Some(shaft));
        assert_eq!(WorldView::block_state_at(&c, [5, 5, 5]), None);
        // Writes rebuild the cluster and keep palette invariants.
        assert!(c.set_block_state([1, 0, 0], shaft));
        assert_eq!(c.block_count(), 2);
        assert_eq!(WorldView::block_state_at(&c, [1, 0, 0]), Some(shaft));
        // The transform is the rigid pose with the pivot at the local
        // origin: local cell 0 maps straight onto the anchor.
        let t = WorldView::world_transform(&c);
        assert_eq!(t.pivot, [0.0; 3]);
        let w = t.local_to_world([0.0, 0.0, 0.0]).unwrap();
        assert_eq!(w, [10.0, 64.0, 10.0]);
    }
}
