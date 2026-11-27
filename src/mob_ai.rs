// R7 GP-1: Minecraft 1.21.1 land-mob navigation and goal scheduling.
// Included in main.rs at crate scope. World lookups are supplied by the caller.

use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum BlockPathType {
    Blocked,
    Open,
    Walkable,
    #[cfg(test)]
    WalkableDoor,
    Trapdoor,
    PowderSnow,
    DangerPowdersnow,
    Fence,
    Lava,
    Water,
    WaterBorder,
    Rail,
    #[cfg(test)]
    UnpassableRail,
    DangerFire,
    DamageFire,
    DangerOther,
    DamageOther,
    DoorOpen,
    DoorWoodClosed,
    DoorIronClosed,
    #[cfg(test)]
    Breach,
    Leaves,
    StickyHoney,
    Cocoa,
    DamageCautious,
    DangerTrapdoor,
}

// PathType's default values are independent of a mob's per-type overrides.
pub(crate) fn default_malus(path_type: BlockPathType) -> f32 {
    use BlockPathType::*;
    match path_type {
        Blocked | PowderSnow | Fence | Lava | DamageOther | DoorWoodClosed
        | DoorIronClosed | Leaves => -1.0,
        #[cfg(test)]
        UnpassableRail => -1.0,
        Water | WaterBorder | DangerFire | DangerOther | StickyHoney => 8.0,
        DamageFire => 16.0,
        #[cfg(test)]
        Breach => 4.0,
        Open | Walkable | Trapdoor | DangerPowdersnow | Rail | DoorOpen | Cocoa
        | DamageCautious | DangerTrapdoor => 0.0,
        #[cfg(test)]
        WalkableDoor => 0.0,
    }
}

pub(crate) trait PathWorld {
    fn path_type(&self, x: i32, y: i32, z: i32) -> BlockPathType;
    fn is_solid(&self, x: i32, y: i32, z: i32) -> bool;
    fn collision_height(&self, x: i32, y: i32, z: i32) -> f64;
    fn min_y(&self) -> i32;
    fn max_y(&self) -> i32;

    fn waypoint_y(&self, _x: i32, y: i32, _z: i32) -> f64 {
        f64::from(y)
    }

    /// `maxEntityCramming` census around `pos`: `(pushable, non_passengers,
    /// roll_mod_4)` — entities sharing the cell, those riding nothing, and a
    /// scripted roll already reduced mod 4 (a multiple of 4 lands the hit).
    /// Worlds that cannot count neighbouring entities report `None`, which
    /// keeps cramming damage at zero exactly like the live push engine's
    /// empty candidate set.
    fn cramming_census(&self, _x: f64, _y: f64, _z: f64) -> Option<(usize, usize, u32)> {
        None
    }

    /// Whether the block at the cell stops daylight. Worlds without a light
    /// channel approximate `Blocks.lightBlock` with the motion-blocking flag.
    fn sky_occluded(&self, x: i32, y: i32, z: i32) -> bool {
        self.is_solid(x, y, z)
    }

    /// `LevelReader.canSeeSky`: no daylight-blocking cell at or above
    /// `(x, y, z)`. Cells past the build ceiling are open sky.
    fn sees_sky(&self, x: i32, y: i32, z: i32) -> bool {
        let mut scan = y;
        loop {
            if scan > self.max_y() || self.sky_occluded(x, scan, z) {
                return scan > self.max_y();
            }
            match scan.checked_add(1) {
                Some(above) => scan = above,
                None => return true,
            }
        }
    }

    /// Protocol block name at the cell, or `"minecraft:air"` when the world
    /// cannot name blocks (the registry's own fallback for missing states).
    fn block_name(&self, _x: i32, _y: i32, _z: i32) -> &str {
        "minecraft:air"
    }
}

/// Classifies a single state as in 1.21.1 WalkNodeEvaluator.getPathTypeFromState.
pub(crate) fn classify_block_state_path_type(
    reg: &crate::block_registry::BReg,
    state: u32,
) -> BlockPathType {
    use BlockPathType::*;
    if reg.is_air(state) {
        return Open;
    }
    let name = reg.name_of(state);
    if reg.in_tag(state, "minecraft:trapdoors") {
        return Trapdoor;
    }
    // These block-specific rules have no corresponding vanilla pathfinding tags.
    match name {
        "minecraft:lily_pad" | "minecraft:big_dripleaf" => return Trapdoor,
        "minecraft:powder_snow" => return PowderSnow,
        "minecraft:cactus" | "minecraft:sweet_berry_bush" => return DamageOther,
        "minecraft:honey_block" => return StickyHoney,
        "minecraft:cocoa" => return Cocoa,
        "minecraft:wither_rose" | "minecraft:pointed_dripstone" => return DamageCautious,
        _ => {}
    }
    let fluid = reg.fluid_name(state);
    if matches!(fluid, "minecraft:lava" | "minecraft:flowing_lava") {
        return Lava;
    }
    if reg.in_tag(state, "minecraft:fire")
        || name == "minecraft:magma_block"
        || name == "minecraft:lava_cauldron"
        || (reg.in_tag(state, "minecraft:campfires")
            && reg.get_prop(state, "lit").as_deref() == Some("true"))
    {
        return DamageFire;
    }
    if reg.in_tag(state, "minecraft:doors") {
        return if reg.get_prop(state, "open").as_deref() == Some("true") {
            DoorOpen
        } else if reg.in_tag(state, "minecraft:mob_interactable_doors") {
            DoorWoodClosed
        } else {
            DoorIronClosed
        };
    }
    if reg.in_tag(state, "minecraft:rails") {
        return Rail;
    }
    if reg.in_tag(state, "minecraft:leaves") {
        return Leaves;
    }
    if reg.in_tag(state, "minecraft:fences")
        || reg.in_tag(state, "minecraft:walls")
        || (reg.in_tag(state, "minecraft:fence_gates")
            && reg.get_prop(state, "open").as_deref() == Some("false"))
    {
        return Fence;
    }
    if !land_pathfindable(reg, state, name) {
        return Blocked;
    }
    if reg.has_water(state) {
        Water
    } else {
        Open
    }
}

// LAND dispatch for the states which reach the fallback in getPathTypeFromState.
// For those states, the pinned 1.21.1 collision-emptiness predicate agrees with
// LAND except for moving pistons and the passable partial shapes below.
fn land_pathfindable(reg: &crate::block_registry::BReg, state: u32, name: &str) -> bool {
    if reg.is_collision_empty(state) {
        return name != "minecraft:moving_piston";
    }
    if name == "minecraft:snow" {
        return reg
            .get_prop(state, "layers")
            .and_then(|layers| layers.parse::<u8>().ok())
            .is_some_and(|layers| layers < 5);
    }
    reg.in_tag(state, "minecraft:climbable")
        || reg.in_tag(state, "minecraft:candles")
        || reg.in_tag(state, "minecraft:wool_carpets")
        || matches!(
            name,
            "minecraft:amethyst_cluster"
                | "minecraft:azalea"
                | "minecraft:comparator"
                | "minecraft:daylight_detector"
                | "minecraft:flowering_azalea"
                | "minecraft:large_amethyst_bud"
                | "minecraft:medium_amethyst_bud"
                | "minecraft:moss_carpet"
                | "minecraft:pitcher_crop"
                | "minecraft:repeater"
                | "minecraft:sculk_shrieker"
                | "minecraft:small_amethyst_bud"
                | "minecraft:turtle_egg"
        )
}

/// Reads protocol block states from a dimension or a bounded snapshot.
pub(crate) struct LiveWorldPathAdapter<'a> {
    reg: &'a crate::block_registry::BReg,
    min_y: i32,
    max_y: i32,
    block_getter: Box<dyn Fn(i32, i32, i32) -> u32 + 'a>,
}

impl<'a> LiveWorldPathAdapter<'a> {
    /// Uses the same dimension-aware block lookup as the live mob tick.
    pub fn new(dimension: crate::worldgen::DimId, reg: &'a crate::block_registry::BReg) -> Self {
        Self::with_block_getter(dimension, reg, block_at_cached_dim)
    }

    pub fn with_block_getter(
        dimension: crate::worldgen::DimId,
        reg: &'a crate::block_registry::BReg,
        getter: impl Fn(crate::worldgen::DimId, i32, i32, i32) -> u32 + 'a,
    ) -> Self {
        let min_y = dimension.min_y();
        Self::with_bounds(
            reg,
            min_y,
            min_y + dimension.height() - 1,
            move |x, y, z| getter(dimension, x, y, z),
        )
    }

    /// Both build-height bounds are inclusive.
    pub fn with_bounds(
        reg: &'a crate::block_registry::BReg,
        min_y: i32,
        max_y: i32,
        getter: impl Fn(i32, i32, i32) -> u32 + 'a,
    ) -> Self {
        Self {
            reg,
            min_y,
            max_y,
            block_getter: Box::new(getter),
        }
    }

    fn state_at(&self, x: i32, y: i32, z: i32) -> Option<u32> {
        // The world supplies air outside its build height. Keep the dimension
        // reader inside its overlay index bounds.
        (self.min_y..=self.max_y)
            .contains(&y)
            .then(|| (self.block_getter)(x, y, z))
    }

    fn collision_top(&self, state: u32) -> f64 {
        self.reg
            .collision_boxes(state)
            .map(|(_, max)| max[1])
            .reduce(f64::max)
            .unwrap_or(0.0)
    }

    fn raw_path_type(&self, x: i32, y: i32, z: i32) -> BlockPathType {
        self.state_at(x, y, z).map_or(BlockPathType::Open, |state| {
            classify_block_state_path_type(self.reg, state)
        })
    }

    // Vanilla returns the first matching neighbor in X/Y/Z order and skips
    // the entire center column.
    fn walking_hazard(&self, x: i32, y: i32, z: i32) -> BlockPathType {
        use BlockPathType::*;
        for dx in -1..=1 {
            for dy in -1..=1 {
                let Some(neighbor_y) = y.checked_add(dy) else {
                    continue;
                };
                for dz in -1..=1 {
                    if dx == 0 && dz == 0 {
                        continue;
                    }
                    match self.raw_path_type(x.wrapping_add(dx), neighbor_y, z.wrapping_add(dz)) {
                        DamageOther => return DangerOther,
                        DamageFire | Lava => return DangerFire,
                        Water => return WaterBorder,
                        DamageCautious => return DamageCautious,
                        _ => {}
                    }
                }
            }
        }
        Walkable
    }
}

impl PathWorld for LiveWorldPathAdapter<'_> {
    fn path_type(&self, x: i32, y: i32, z: i32) -> BlockPathType {
        use BlockPathType::*;
        let here = self.raw_path_type(x, y, z);
        if here != Open || y <= self.min_y {
            return here;
        }
        match self.raw_path_type(x, y - 1, z) {
            Open | Water | Lava | Walkable => Open,
            DamageFire => DamageFire,
            DamageOther => DamageOther,
            StickyHoney => StickyHoney,
            PowderSnow => DangerPowdersnow,
            DamageCautious => DamageCautious,
            Trapdoor => DangerTrapdoor,
            _ => self.walking_hazard(x, y, z),
        }
    }

    fn is_solid(&self, x: i32, y: i32, z: i32) -> bool {
        self.state_at(x, y, z)
            .is_some_and(|state| self.reg.blocks_motion(state))
    }

    fn collision_height(&self, x: i32, y: i32, z: i32) -> f64 {
        self.state_at(x, y, z)
            .map_or(0.0, |state| self.collision_top(state))
    }

    fn min_y(&self) -> i32 {
        self.min_y
    }

    fn max_y(&self) -> i32 {
        self.max_y
    }

    fn waypoint_y(&self, x: i32, y: i32, z: i32) -> f64 {
        // PathNavigation.getGroundY keeps the node's Y when the block below is air.
        let Some(below_y) = y.checked_sub(1) else {
            return f64::from(y);
        };
        self.state_at(x, below_y, z)
            .filter(|&state| !self.reg.is_air(state))
            .map_or(f64::from(y), |state| {
                f64::from(below_y) + self.collision_top(state)
            })
    }

    fn sky_occluded(&self, x: i32, y: i32, z: i32) -> bool {
        // The pinned light-opacity channel answers `canSeeSky` exactly:
        // transparent states (glass, air, most plants) do not stop daylight.
        self.state_at(x, y, z)
            .is_some_and(|state| self.reg.opacity_of(state) > 0)
    }

    fn block_name(&self, x: i32, y: i32, z: i32) -> &str {
        self.state_at(x, y, z)
            .map_or("minecraft:air", |state| self.reg.name_of(state))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PathNode {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub path_type: BlockPathType,
    pub malus: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AiPath {
    pub nodes: Vec<PathNode>,
    pub target: (i32, i32, i32),
    pub reached: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct WalkNodeEvaluator {
    pub mob_height: f64,
    pub max_fall_distance: i32,
    /// Villagers/vindicators open, and hard-mode zombies break, wooden doors;
    /// those mobs path through the leaf while their door interaction goal owns
    /// the actual world mutation.
    pub can_pass_closed_wooden_doors: bool,
}

impl Default for WalkNodeEvaluator {
    fn default() -> Self {
        Self {
            mob_height: 1.8,
            max_fall_distance: 3,
            can_pass_closed_wooden_doors: false,
        }
    }
}

impl WalkNodeEvaluator {
    fn height_in_blocks(&self) -> i32 {
        self.mob_height.ceil().max(1.0) as i32
    }

    // A clear body requires each occupied block to be passable. The node's
    // path type comes from the feet position.
    fn clear_body(&self, world: &dyn PathWorld, pos: (i32, i32, i32)) -> bool {
        let (x, y, z) = pos;
        let Some(top) = y.checked_add(self.height_in_blocks() - 1) else {
            return false;
        };
        if y < world.min_y() || top > world.max_y() {
            return false;
        }
        for block_y in y..=top {
            let path_type = world.path_type(x, block_y, z);
            // Ground navigation can pass the swung panel of an open door.
            if path_type == BlockPathType::DoorOpen
                || (self.can_pass_closed_wooden_doors
                    && path_type == BlockPathType::DoorWoodClosed)
            {
                continue;
            }
            if default_malus(path_type) < 0.0
                || world.is_solid(x, block_y, z)
                || world.collision_height(x, block_y, z) > 0.0
            {
                return false;
            }
        }
        true
    }

    fn has_support(&self, world: &dyn PathWorld, pos: (i32, i32, i32)) -> bool {
        let (x, y, z) = pos;
        if y <= world.min_y() {
            return false;
        }
        let below = y - 1;
        world.is_solid(x, below, z)
            || world.collision_height(x, below, z) > 0.0
            || matches!(world.path_type(x, below, z), BlockPathType::Water)
    }

    fn standable(&self, world: &dyn PathWorld, pos: (i32, i32, i32)) -> bool {
        self.clear_body(world, pos) && self.has_support(world, pos)
    }

    fn accepted_node(&self, world: &dyn PathWorld, pos: (i32, i32, i32)) -> PathNode {
        let path_type = world.path_type(pos.0, pos.1, pos.2);
        PathNode {
            x: pos.0,
            y: pos.1,
            z: pos.2,
            path_type,
            malus: default_malus(path_type),
        }
    }

    // Find one legal horizontal step, one-block climb, or bounded safe fall.
    // Checking clearance at each descent level prevents falling through a
    // wall, and checking the source headroom prevents climbing through roofs.
    fn neighbor(
        &self,
        world: &dyn PathWorld,
        from: (i32, i32, i32),
        dx: i32,
        dz: i32,
    ) -> Option<PathNode> {
        let x = from.0.checked_add(dx)?;
        let z = from.2.checked_add(dz)?;
        if self.standable(world, (x, from.1, z)) {
            return Some(self.accepted_node(world, (x, from.1, z)));
        }

        let up = from.1.checked_add(1)?;
        if self.clear_body(world, (from.0, up, from.2)) && self.standable(world, (x, up, z)) {
            return Some(self.accepted_node(world, (x, up, z)));
        }

        // A fall begins through air or fluid, never through a blocked cell.
        if !self.clear_body(world, (x, from.1, z)) {
            return None;
        }
        for drop in 1..=self.max_fall_distance {
            let y = from.1.checked_sub(drop)?;
            if !self.clear_body(world, (x, y, z)) {
                return None;
            }
            if self.standable(world, (x, y, z)) {
                return Some(self.accepted_node(world, (x, y, z)));
            }
        }
        None
    }

    fn neighbors(&self, world: &dyn PathWorld, from: (i32, i32, i32)) -> Vec<PathNode> {
        // Stable enumeration order also keeps equivalent paths reproducible.
        const CARDINALS: [(i32, i32); 4] = [(-1, 0), (0, -1), (0, 1), (1, 0)];
        const DIAGONALS: [(i32, i32); 4] = [(-1, -1), (-1, 1), (1, -1), (1, 1)];
        let mut result = Vec::with_capacity(8);
        for (dx, dz) in CARDINALS {
            if let Some(node) = self.neighbor(world, from, dx, dz) {
                result.push(node);
            }
        }
        for (dx, dz) in DIAGONALS {
            let (Some(x_side), Some(z_side)) = (
                self.neighbor(world, from, dx, 0),
                self.neighbor(world, from, 0, dz),
            ) else {
                continue;
            };
            let Some(diagonal) = self.neighbor(world, from, dx, dz) else {
                continue;
            };
            // Diagonal clearance must hold on both cardinal sides at the
            // diagonal's elevation. This forbids corner cutting by steps/drops.
            if x_side.y == diagonal.y && z_side.y == diagonal.y {
                result.push(diagonal);
            }
        }
        result
    }
}

#[derive(Clone, Copy, Debug)]
struct SearchRecord {
    g_cost: f64,
    parent: Option<(i32, i32, i32)>,
}

#[derive(Clone, Copy, Debug)]
struct SearchQueueEntry {
    position: (i32, i32, i32),
    f_cost: f64,
    h_cost: f64,
    g_cost: f64,
}

impl PartialEq for SearchQueueEntry {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}
impl Eq for SearchQueueEntry {}
impl PartialOrd for SearchQueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for SearchQueueEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // BinaryHeap pops the largest entry. Reverse all comparisons for a
        // minimum heap. Coordinates break equal-cost ties deterministically.
        other
            .f_cost
            .total_cmp(&self.f_cost)
            .then_with(|| other.h_cost.total_cmp(&self.h_cost))
            .then_with(|| other.position.cmp(&self.position))
    }
}

fn path_distance(a: (i32, i32, i32), b: (i32, i32, i32)) -> f64 {
    let dx = f64::from(a.0) - f64::from(b.0);
    let dy = f64::from(a.1) - f64::from(b.1);
    let dz = f64::from(a.2) - f64::from(b.2);
    (dx * dx + dy * dy + dz * dz).sqrt()
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PathFinder {
    pub evaluator: WalkNodeEvaluator,
    pub max_visited_nodes: usize,
}

impl Default for PathFinder {
    fn default() -> Self {
        Self {
            evaluator: WalkNodeEvaluator::default(),
            max_visited_nodes: 1024,
        }
    }
}

impl PathFinder {
    pub fn find_path(
        &self,
        world: &dyn PathWorld,
        start: (i32, i32, i32),
        target: (i32, i32, i32),
    ) -> AiPath {
        if !self.evaluator.standable(world, start) {
            return AiPath {
                nodes: Vec::new(),
                target,
                reached: false,
            };
        }
        let mut records = BTreeMap::new();
        let mut expanded = BTreeSet::new();
        let mut open = BinaryHeap::new();
        let h = path_distance(start, target);
        records.insert(
            start,
            SearchRecord {
                g_cost: 0.0,
                parent: None,
            },
        );
        open.push(SearchQueueEntry {
            position: start,
            f_cost: h,
            h_cost: h,
            g_cost: 0.0,
        });
        let mut closest = start;
        let mut closest_distance = h;
        let mut closest_cost = 0.0;
        let mut reached = false;
        while expanded.len() < self.max_visited_nodes {
            let Some(entry) = open.pop() else { break };
            let Some(record) = records.get(&entry.position) else {
                continue;
            };
            if expanded.contains(&entry.position) || entry.g_cost > record.g_cost {
                continue;
            }
            expanded.insert(entry.position);
            let distance = path_distance(entry.position, target);
            if distance < closest_distance
                || (distance == closest_distance
                    && (entry.g_cost < closest_cost
                        || (entry.g_cost == closest_cost && entry.position < closest)))
            {
                closest = entry.position;
                closest_distance = distance;
                closest_cost = entry.g_cost;
            }
            if entry.position == target {
                reached = true;
                break;
            }
            for neighbor in self.evaluator.neighbors(world, entry.position) {
                let pos = (neighbor.x, neighbor.y, neighbor.z);
                if expanded.contains(&pos) {
                    continue;
                }
                let cost =
                    entry.g_cost + path_distance(entry.position, pos) + f64::from(neighbor.malus);
                let better = records.get(&pos).is_none_or(|old| cost < old.g_cost);
                if better {
                    records.insert(
                        pos,
                        SearchRecord {
                            g_cost: cost,
                            parent: Some(entry.position),
                        },
                    );
                    let h = path_distance(pos, target);
                    open.push(SearchQueueEntry {
                        position: pos,
                        f_cost: cost + h,
                        h_cost: h,
                        g_cost: cost,
                    });
                }
            }
        }
        let mut nodes = Vec::new();
        let mut current = closest;
        loop {
            nodes.push(self.evaluator.accepted_node(world, current));
            match records.get(&current).and_then(|record| record.parent) {
                Some(parent) => current = parent,
                None => break,
            }
        }
        nodes.reverse();
        AiPath {
            nodes,
            target,
            reached,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PathNavigation {
    path: Option<AiPath>,
    next_node: usize,
}

impl PathNavigation {
    pub fn navigate_to(
        &mut self,
        finder: &PathFinder,
        world: &dyn PathWorld,
        start: (i32, i32, i32),
        target: (i32, i32, i32),
    ) -> bool {
        let path = finder.find_path(world, start, target);
        let reached = path.reached;
        self.next_node = usize::from(path.nodes.len() > 1);
        self.path = Some(path);
        reached
    }

    pub fn next_waypoint(&self, world: &dyn PathWorld) -> Option<(f64, f64, f64)> {
        let node = self.path.as_ref()?.nodes.get(self.next_node)?;
        Some((
            f64::from(node.x) + 0.5,
            world.waypoint_y(node.x, node.y, node.z),
            f64::from(node.z) + 0.5,
        ))
    }

    pub fn advance_if_reached(
        &mut self,
        world: &dyn PathWorld,
        position: (f64, f64, f64),
        tolerance: f64,
    ) {
        if let Some(waypoint) = self.next_waypoint(world) {
            let dx = position.0 - waypoint.0;
            let dy = position.1 - waypoint.1;
            let dz = position.2 - waypoint.2;
            if dx * dx + dy * dy + dz * dz <= tolerance * tolerance {
                self.next_node += 1;
            }
        }
    }

    pub fn is_done(&self) -> bool {
        self.path
            .as_ref()
            .is_none_or(|path| self.next_node >= path.nodes.len())
    }

    pub fn stop(&mut self) {
        self.path = None;
        self.next_node = 0;
    }

    #[cfg(test)]
    pub fn path(&self) -> Option<&AiPath> {
        self.path.as_ref()
    }
}

pub(crate) struct GoalFlags;

impl GoalFlags {
    pub const MOVE: u8 = 1;
    pub const LOOK: u8 = 2;
    pub const JUMP: u8 = 4;
    pub const TARGET: u8 = 8;
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct AiEntityPerception {
    pub entity_id: i32,
    /// Protocol entity type id; `-1` when the caller does not classify it.
    pub etype: i64,
    pub position: (f64, f64, f64),
    pub width: f32,
    pub line_of_sight: bool,
    pub held_item: Option<u32>,
    pub in_love: bool,
    pub compatible: bool,
}

impl AiEntityPerception {
    pub fn new(entity_id: i32, position: (f64, f64, f64)) -> Self {
        Self {
            entity_id,
            etype: -1,
            position,
            width: 0.6,
            line_of_sight: true,
            held_item: None,
            in_love: false,
            compatible: false,
        }
    }
}

/// One mob observed next to the ticking entity. Mob-vs-mob goals (fleeing,
/// inter-mob targeting) filter these; players keep arriving through
/// `AiEntityPerception` instead.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct NearbyMob {
    pub entity_id: i32,
    /// Protocol entity type id (`entity_spec_by_id` index).
    pub etype: i64,
    pub position: (f64, f64, f64),
    pub line_of_sight: bool,
    /// Rolled-up armadillo-style state; vanilla spiders only flee armadillos
    /// that are *not* scared.
    pub scared: bool,
    /// Vanilla `isBaby`; foxes only prey on turtles while they are babies.
    pub baby: bool,
}

impl NearbyMob {
    #[cfg(test)]
    pub fn new(entity_id: i32, etype: i64, position: (f64, f64, f64)) -> Self {
        Self {
            entity_id,
            etype,
            position,
            line_of_sight: true,
            scared: false,
            baby: false,
        }
    }
}

/// The per-tick mob-vs-mob observation shared with every goal. `nearby_mobs`
/// defaults to empty so callers that supply no mob perception see no change.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct MobPerception {
    pub nearby_mobs: Vec<NearbyMob>,
}

impl MobPerception {
    /// Nearest mob satisfying `accept` within `max_dist` of `position`.
    fn nearest(
        &self,
        position: (f64, f64, f64),
        max_dist: f64,
        accept: impl Fn(&NearbyMob) -> bool,
    ) -> Option<NearbyMob> {
        let max_dist_sq = max_dist * max_dist;
        self.nearby_mobs
            .iter()
            .filter(|mob| accept(mob))
            .filter(|mob| entity_distance_sq(position, mob.position) <= max_dist_sq)
            .min_by(|a, b| {
                entity_distance_sq(position, a.position)
                    .total_cmp(&entity_distance_sq(position, b.position))
            })
            .copied()
    }
}

pub(crate) struct GoalContext<'a> {
    pub position: (f64, f64, f64),
    pub width: f32,
    pub height: f64,
    pub in_water: bool,
    pub in_lava: bool,
    pub on_fire: bool,
    pub recently_hurt: bool,
    pub in_love: bool,
    pub target: Option<AiEntityPerception>,
    pub nearest_player: Option<AiEntityPerception>,
    pub mate: Option<AiEntityPerception>,
    pub world: Option<&'a dyn PathWorld>,
}

impl Default for GoalContext<'_> {
    fn default() -> Self {
        Self {
            position: (0.0, 0.0, 0.0),
            width: 0.6,
            height: 1.8,
            in_water: false,
            in_lava: false,
            on_fire: false,
            recently_hurt: false,
            in_love: false,
            target: None,
            nearest_player: None,
            mate: None,
            world: None,
        }
    }
}

fn entity_distance_sq(a: (f64, f64, f64), b: (f64, f64, f64)) -> f64 {
    let dx = a.0 - b.0;
    let dy = a.1 - b.1;
    let dz = a.2 - b.2;
    dx * dx + dy * dy + dz * dz
}

/// The closest cell accepted by `accept` in a `±horizontal` x `±vertical` box
/// around the mob — the shared body of the vanilla block searches
/// (`findNearestBlock`, `DoorInteractGoal`). Cells are visited y/x/z and ties on
/// squared distance keep the first hit, which is the scan order those goals use.
fn nearest_cell(
    ctx: &GoalContext<'_>,
    horizontal: i32,
    vertical: i32,
    accept: impl Fn(&dyn PathWorld, (i32, i32, i32)) -> bool,
) -> Option<(i32, i32, i32)> {
    let world = ctx.world?;
    let (cx, cy, cz) = (
        ctx.position.0.floor() as i32,
        ctx.position.1.floor() as i32,
        ctx.position.2.floor() as i32,
    );
    let mut best: Option<((i32, i32, i32), i64)> = None;
    for dy in -vertical..=vertical {
        for dx in -horizontal..=horizontal {
            for dz in -horizontal..=horizontal {
                let (Some(x), Some(y), Some(z)) =
                    (cx.checked_add(dx), cy.checked_add(dy), cz.checked_add(dz))
                else {
                    continue;
                };
                if !accept(world, (x, y, z)) {
                    continue;
                }
                let distance_sq = i64::from(dx) * i64::from(dx)
                    + i64::from(dy) * i64::from(dy)
                    + i64::from(dz) * i64::from(dz);
                if best.is_none_or(|(_, old)| distance_sq < old) {
                    best = Some(((x, y, z), distance_sq));
                }
            }
        }
    }
    best.map(|(pos, _)| pos)
}

/// Whether a block cell's centre, raised `dy` above the cell floor, lies within
/// `radius_sq` of the mob.
fn cell_within(ctx: &GoalContext<'_>, cell: (i32, i32, i32), dy: f64, radius_sq: f64) -> bool {
    entity_distance_sq(
        ctx.position,
        (
            f64::from(cell.0) + 0.5,
            f64::from(cell.1) + dy,
            f64::from(cell.2) + 0.5,
        ),
    ) <= radius_sq
}

/// A projectile launch a ranged goal wants the caller to perform. The caller
/// owns spawning, spread/inaccuracy, and damage application.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum RangedProjectileAction {
    /// Vanilla `SnowGolem.performRangedAttack` (snowball arc, 1.6 speed).
    Snowball {
        target_eid: i32,
        target_pos: (f64, f64, f64),
        velocity: (f64, f64, f64),
    },
    /// Vanilla `AbstractSkeleton.performRangedAttack` (arrow arc, 1.6 speed).
    /// Strays loose tipped slowness arrows, bogged tipped poison arrows; a
    /// plain skeleton releases `None`.
    Arrow {
        target_eid: i32,
        target_pos: (f64, f64, f64),
        velocity: (f64, f64, f64),
        tipped: Option<&'static str>,
    },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct GoalOutput {
    pub move_target: Option<(f64, f64, f64)>,
    pub look_target: Option<(f64, f64, f64)>,
    /// Multiplies the mob's waypoint stride (vanilla walk/sprint modifiers).
    pub speed_modifier: Option<f64>,
    pub jump: bool,
    pub attack_target: Option<i32>,
    /// Mob-vs-mob target chosen by a target-selector goal this tick.
    pub mob_target: Option<i32>,
    /// Projectile a ranged goal wants launched this tick.
    pub ranged_attack: Option<RangedProjectileAction>,
    pub swell: Option<bool>,
    pub breed_target: Option<i32>,
    /// XZ velocity override from a goal that owns the mob's momentum this
    /// tick (LeapAtTargetGoal pounce, RangedBowAttackGoal strafe); replaces
    /// the waypoint stride.
    pub move_impulse: Option<(f64, f64)>,
    /// Block cell a goal wants removed (turtle-egg stomp, door break finish).
    pub destroy_block: Option<(i32, i32, i32)>,
    /// BreakDoorGoal's `destroyBlockProgress` broadcast: door cell plus the
    /// vanilla crack stage 0..10.
    pub door_break_progress: Option<((i32, i32, i32), u8)>,
    /// Zombie `setAggressive` arm-raise state while attacking.
    pub arms_raised: Option<bool>,
    /// RestrictSunGoal's navigation sun-avoidance restriction.
    pub restrict_sun: Option<bool>,
    /// Closed wooden door cell an OpenDoorGoal wants toggled open.
    pub open_door: Option<(i32, i32, i32)>,
}

impl GoalOutput {
    // Outputs are merged in priority order. Only the first owner of each
    // command channel may write it, except that jump is an additive request.
    fn merge_missing(&mut self, next: GoalOutput) {
        self.move_target = self.move_target.or(next.move_target);
        self.look_target = self.look_target.or(next.look_target);
        self.speed_modifier = self.speed_modifier.or(next.speed_modifier);
        self.jump |= next.jump;
        self.attack_target = self.attack_target.or(next.attack_target);
        self.mob_target = self.mob_target.or(next.mob_target);
        self.ranged_attack = self.ranged_attack.or(next.ranged_attack);
        self.swell = self.swell.or(next.swell);
        self.breed_target = self.breed_target.or(next.breed_target);
        self.move_impulse = self.move_impulse.or(next.move_impulse);
        self.destroy_block = self.destroy_block.or(next.destroy_block);
        self.door_break_progress = self.door_break_progress.or(next.door_break_progress);
        self.arms_raised = self.arms_raised.or(next.arms_raised);
        self.restrict_sun = self.restrict_sun.or(next.restrict_sun);
        self.open_door = self.open_door.or(next.open_door);
    }
}

pub(crate) trait Goal {
    fn priority(&self) -> i32;
    fn flags(&self) -> u8;
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool;
    /// Vanilla goals keep running while `canUse` still holds unless they track
    /// extra state; that common case needs no override.
    fn can_continue_to_use(&self, ctx: &GoalContext<'_>) -> bool {
        self.can_use(ctx)
    }
    fn is_interruptable(&self) -> bool {
        true
    }
    fn start(&mut self, ctx: &GoalContext<'_>);
    fn tick(&mut self, ctx: &GoalContext<'_>) -> Option<GoalOutput>;
    fn stop(&mut self, ctx: &GoalContext<'_>);
    fn idle_tick(&mut self, _ctx: &GoalContext<'_>) {}
    /// Receives the mob-vs-mob observation before `can_use` evaluation runs.
    /// Goals that never look at neighbouring mobs keep the default no-op.
    fn perceive(&mut self, _position: (f64, f64, f64), _perception: &MobPerception) {}
    fn stop_output(&self) -> Option<GoalOutput> {
        None
    }
}

struct RunningGoal {
    goal: Box<dyn Goal>,
    running: bool,
}

/// The two `Goal` accessors every goal repeats verbatim: the registration
/// priority it stores and the vanilla mutex flag mask of its goal class.
macro_rules! goal_accessors {
    ($flags:expr) => {
        fn priority(&self) -> i32 {
            self.priority
        }
        fn flags(&self) -> u8 {
            $flags
        }
    };
}

/// `Goal` accessors for a wrapper that inherits its inner goal's class.
macro_rules! delegated_accessors {
    ($field:ident) => {
        fn priority(&self) -> i32 {
            self.$field.priority
        }
        fn flags(&self) -> u8 {
            self.$field.flags()
        }
    };
}

/// `Goal` lifecycle passthroughs for the wrappers that decorate an inner goal;
/// a wrapper only spells out the methods it changes.
macro_rules! delegated_lifecycle {
    () => {
        fn start(&mut self, ctx: &GoalContext<'_>) {
            self.inner.start(ctx);
        }
        fn stop(&mut self, ctx: &GoalContext<'_>) {
            self.inner.stop(ctx);
        }
    };
}

#[derive(Default)]
pub(crate) struct GoalSelector {
    goals: Vec<RunningGoal>,
    /// Mob-vs-mob observation applied to every goal at the top of `tick`.
    /// Empty by default so selectors without a perception feed are unchanged.
    pub perception: MobPerception,
}

impl GoalSelector {
    pub fn add_goal(&mut self, goal: Box<dyn Goal>) {
        self.goals.push(RunningGoal {
            goal,
            running: false,
        });
        // Stable sorting retains registration order for equal priorities.
        self.goals.sort_by_key(|entry| entry.goal.priority());
    }

    #[cfg(test)]
    pub fn running_count(&self) -> usize {
        self.goals.iter().filter(|entry| entry.running).count()
    }

    pub fn tick(&mut self, ctx: &GoalContext<'_>) -> GoalOutput {
        // Selector-less mobs skip the schedule entirely.
        if self.goals.is_empty() {
            return GoalOutput::default();
        }
        for entry in &mut self.goals {
            entry.goal.perceive(ctx.position, &self.perception);
        }
        let mut stopped = GoalOutput::default();
        for entry in &mut self.goals {
            if entry.running && !entry.goal.can_continue_to_use(ctx) {
                if let Some(output) = entry.goal.stop_output() {
                    stopped.merge_missing(output);
                }
                entry.goal.stop(ctx);
                entry.running = false;
            }
        }
        for entry in &mut self.goals {
            if !entry.running {
                entry.goal.idle_tick(ctx);
            }
        }
        for index in 0..self.goals.len() {
            if self.goals[index].running {
                continue;
            }
            let priority = self.goals[index].goal.priority();
            let flags = self.goals[index].goal.flags();
            let conflicting: Vec<usize> = self
                .goals
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.running && entry.goal.flags() & flags != 0)
                .map(|(i, _)| i)
                .collect();
            if conflicting.iter().any(|&i| {
                self.goals[i].goal.priority() <= priority || !self.goals[i].goal.is_interruptable()
            }) {
                continue;
            }
            if !self.goals[index].goal.can_use(ctx) {
                continue;
            }
            for i in conflicting {
                if let Some(output) = self.goals[i].goal.stop_output() {
                    stopped.merge_missing(output);
                }
                self.goals[i].goal.stop(ctx);
                self.goals[i].running = false;
            }
            self.goals[index].goal.start(ctx);
            self.goals[index].running = true;
        }
        let mut output = GoalOutput::default();
        for entry in &mut self.goals {
            if entry.running {
                if let Some(next) = entry.goal.tick(ctx) {
                    output.merge_missing(next);
                }
            }
        }
        output.merge_missing(stopped);
        output
    }

    #[cfg(test)]
    pub fn stop_all(&mut self, ctx: &GoalContext<'_>) -> GoalOutput {
        let mut output = GoalOutput::default();
        for entry in &mut self.goals {
            if entry.running {
                if let Some(next) = entry.goal.stop_output() {
                    output.merge_missing(next);
                }
                entry.goal.stop(ctx);
                entry.running = false;
            }
        }
        output
    }
}

// Vanilla maintains separate schedulers for goals and targeting, with the
// same priority and flag-mutex rules but independent running state.
#[derive(Default)]
pub(crate) struct TargetSelector {
    selector: GoalSelector,
    /// Mob-vs-mob observation forwarded into the wrapped selector on `tick`.
    pub perception: MobPerception,
}

impl TargetSelector {
    pub fn add_goal(&mut self, goal: Box<dyn Goal>) {
        self.selector.add_goal(goal);
    }

    pub fn tick(&mut self, ctx: &GoalContext<'_>) -> GoalOutput {
        self.selector.perception.clone_from(&self.perception);
        self.selector.tick(ctx)
    }

    #[cfg(test)]
    pub fn running_count(&self) -> usize {
        self.selector.running_count()
    }

    #[cfg(test)]
    pub fn stop_all(&mut self, ctx: &GoalContext<'_>) -> GoalOutput {
        self.selector.stop_all(ctx)
    }
}

#[derive(Debug)]
pub(crate) struct FloatGoal {
    pub priority: i32,
}

impl Goal for FloatGoal {
    goal_accessors!(GoalFlags::JUMP);
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool {
        ctx.in_water || ctx.in_lava
    }
    fn start(&mut self, _: &GoalContext<'_>) {}
    fn tick(&mut self, _: &GoalContext<'_>) -> Option<GoalOutput> {
        Some(GoalOutput {
            jump: true,
            ..GoalOutput::default()
        })
    }
    fn stop(&mut self, _: &GoalContext<'_>) {}
}

#[derive(Debug)]
pub(crate) struct SwellGoal {
    pub priority: i32,
    ignited: bool,
    /// Entity types whose presence inside `avoid_dist` forces the fuse off —
    /// the creeper's cat/ocelot flee outranks swelling in vanilla.
    avoid_etypes: &'static [i64],
    avoid_dist: f32,
    avoid_threat: bool,
}

impl SwellGoal {
    pub fn new(priority: i32) -> Self {
        Self {
            priority,
            ignited: false,
            avoid_etypes: &[],
            avoid_dist: 0.0,
            avoid_threat: false,
        }
    }

    /// While any `etypes` mob is perceived inside `dist`, the goal cannot run
    /// and a running fuse is aborted (vanilla creeper vs cats/ocelots).
    pub fn aborted_by(mut self, etypes: &'static [i64], dist: f32) -> Self {
        self.avoid_etypes = etypes;
        self.avoid_dist = dist;
        self
    }
}

impl Goal for SwellGoal {
    goal_accessors!(GoalFlags::MOVE);
    fn perceive(&mut self, position: (f64, f64, f64), perception: &MobPerception) {
        let max_sq = f64::from(self.avoid_dist) * f64::from(self.avoid_dist);
        self.avoid_threat = !self.avoid_etypes.is_empty()
            && perception.nearby_mobs.iter().any(|mob| {
                self.avoid_etypes.contains(&mob.etype)
                    && entity_distance_sq(position, mob.position) <= max_sq
            });
    }
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool {
        !self.avoid_threat
            && ctx
                .target
                .is_some_and(|target| entity_distance_sq(ctx.position, target.position) < 9.0)
    }
    fn can_continue_to_use(&self, ctx: &GoalContext<'_>) -> bool {
        !self.avoid_threat && (self.ignited || self.can_use(ctx))
    }
    fn start(&mut self, _: &GoalContext<'_>) {}
    fn tick(&mut self, ctx: &GoalContext<'_>) -> Option<GoalOutput> {
        self.ignited = ctx.target.is_some_and(|target| {
            target.line_of_sight && entity_distance_sq(ctx.position, target.position) <= 49.0
        });
        Some(GoalOutput {
            swell: Some(self.ignited),
            ..GoalOutput::default()
        })
    }
    fn stop_output(&self) -> Option<GoalOutput> {
        self.ignited.then(|| GoalOutput {
            swell: Some(false),
            ..GoalOutput::default()
        })
    }
    fn stop(&mut self, _: &GoalContext<'_>) {
        self.ignited = false;
    }
}

#[derive(Debug)]
pub(crate) struct MeleeAttackGoal {
    pub priority: i32,
    attack_cooldown: u32,
}

impl MeleeAttackGoal {
    pub fn new(priority: i32) -> Self {
        Self {
            priority,
            attack_cooldown: 0,
        }
    }
}

impl Goal for MeleeAttackGoal {
    goal_accessors!(GoalFlags::MOVE | GoalFlags::LOOK);
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool {
        ctx.target.is_some()
    }
    fn start(&mut self, _: &GoalContext<'_>) {}
    fn tick(&mut self, ctx: &GoalContext<'_>) -> Option<GoalOutput> {
        self.attack_cooldown = self.attack_cooldown.saturating_sub(1);
        let target = ctx.target?;
        let reach_sq = f64::from((ctx.width * 2.0).powi(2) + target.width);
        let in_reach = entity_distance_sq(ctx.position, target.position) <= reach_sq;
        let attack_target = if in_reach && self.attack_cooldown == 0 {
            self.attack_cooldown = 20;
            Some(target.entity_id)
        } else {
            None
        };
        Some(GoalOutput {
            move_target: (!in_reach).then_some(target.position),
            look_target: Some(target.position),
            attack_target,
            ..GoalOutput::default()
        })
    }
    fn stop(&mut self, _: &GoalContext<'_>) {
        self.attack_cooldown = 0;
    }
}

/// Shared splitmix64 stream a goal advances in place of vanilla's `Random`
/// draws — the same mixer `WaterAvoidingRandomStrollGoal::next_random` uses.
fn goal_seed_draw(seed: &mut u64) -> u64 {
    *seed = seed.wrapping_add(0x9e3779b97f4a7c15);
    let mut value = *seed;
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

/// `Level.isDay` as the live mob tick computes it for the overworld.
fn overworld_is_day() -> bool {
    let wt = WORLD_TIME.load(Ordering::Relaxed) % 24000;
    !(13000..=23000).contains(&wt)
}

/// Hard `Difficulty` as the live server stores it (`difficulty` = 3).
fn difficulty_is_hard() -> bool {
    DIFFICULTY.load(Ordering::Relaxed) == 3
}

/// World-rule predicate a goal re-reads on each evaluation — `isDay`,
/// `mobGriefing`, or the difficulty gate. Plan construction binds the live
/// rule; tests inject constants so goal behavior stays deterministic.
#[derive(Clone, Copy)]
pub(crate) struct RuleProbe(pub fn() -> bool);

impl RuleProbe {
    fn check(&self) -> bool {
        (self.0)()
    }
}

impl std::fmt::Debug for RuleProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RuleProbe")
    }
}

/// The undead sun rule's two vanilla clauses — `Level.isDay` plus
/// `Level.canSeeSky` at the mob's feet — evaluated against the tick's world.
fn mob_in_open_daylight(ctx: &GoalContext<'_>, daytime: RuleProbe) -> bool {
    daytime.check()
        && ctx.world.is_some_and(|world| {
            world.sees_sky(
                ctx.position.0.floor() as i32,
                ctx.position.1.floor() as i32,
                ctx.position.2.floor() as i32,
            )
        })
}

/// Vanilla `RandomLookAroundGoal` (1.21.1): every eligible tick a 2% roll
/// arms the goal; once started it holds a random horizontal look direction
/// for `20 + nextInt(20)` ticks. Flags `MOVE|LOOK` as in vanilla.
#[derive(Debug)]
pub(crate) struct RandomLookAroundGoal {
    pub priority: i32,
    /// Mob eye height; the look point shares it so the pitch stays level.
    eye_height: f64,
    seed: u64,
    /// The armed 2% roll, drawn once per idle tick like vanilla's `canUse`.
    armed: bool,
    rel: (f64, f64),
    look_time: i32,
}

impl RandomLookAroundGoal {
    pub fn new(priority: i32, eye_height: f64, seed: u64) -> Self {
        Self {
            priority,
            eye_height,
            seed,
            armed: false,
            rel: (1.0, 0.0),
            look_time: 0,
        }
    }
}

impl Goal for RandomLookAroundGoal {
    goal_accessors!(GoalFlags::MOVE | GoalFlags::LOOK);
    fn can_use(&self, _: &GoalContext<'_>) -> bool {
        self.armed
    }
    fn can_continue_to_use(&self, _: &GoalContext<'_>) -> bool {
        self.look_time >= 0
    }
    fn start(&mut self, _: &GoalContext<'_>) {
        // Vanilla picks a uniform angle on the unit circle and looks from the
        // mob toward `pos + (cos, eyeY, sin)`.
        let angle = (goal_seed_draw(&mut self.seed) >> 11) as f64
            * (std::f64::consts::TAU / f64::from(1_u32 << 21));
        self.rel = (angle.cos(), angle.sin());
        self.look_time = 20 + (goal_seed_draw(&mut self.seed) % 20) as i32;
        self.armed = false;
    }
    fn tick(&mut self, ctx: &GoalContext<'_>) -> Option<GoalOutput> {
        self.look_time -= 1;
        Some(GoalOutput {
            look_target: Some((
                ctx.position.0 + self.rel.0,
                ctx.position.1 + self.eye_height,
                ctx.position.2 + self.rel.1,
            )),
            ..GoalOutput::default()
        })
    }
    fn stop(&mut self, _: &GoalContext<'_>) {
        self.look_time = 0;
    }
    fn idle_tick(&mut self, _: &GoalContext<'_>) {
        // vanilla `mob.getRandom().nextFloat() < 0.02`
        self.armed = goal_seed_draw(&mut self.seed).is_multiple_of(50);
    }
}

/// Vanilla `LeapAtTargetGoal` (1.21.1): with a target in the [2.0, 4.0] block
/// horizontal band each eligible tick rolls 1-in-5 to pounce — 0.4 upward
/// plus a 0.4 forward impulse. The engine models velocity as per-tick
/// impulses, so the impulse re-applies for `LEAP_IMPULSE_TICKS` ticks to
/// approximate the airborne arc vanilla's one-shot `setDeltaMovement` makes.
#[derive(Debug)]
pub(crate) struct LeapAtTargetGoal {
    pub priority: i32,
    seed: u64,
    /// The armed 1-in-5 roll, drawn once per idle tick like vanilla's
    /// `canUse` roll (`nextInt(5) == 0`).
    armed: bool,
    /// Ticks of pounce impulse still to emit.
    leap_ticks: u8,
}

const LEAP_IMPULSE_TICKS: u8 = 3;

impl LeapAtTargetGoal {
    pub fn new(priority: i32, seed: u64) -> Self {
        Self {
            priority,
            seed,
            armed: false,
            leap_ticks: 0,
        }
    }

    fn horizontal_distance_sq(&self, ctx: &GoalContext<'_>) -> Option<f64> {
        let target = ctx.target?;
        let dx = target.position.0 - ctx.position.0;
        let dz = target.position.2 - ctx.position.2;
        Some(dx * dx + dz * dz)
    }
}

impl Goal for LeapAtTargetGoal {
    goal_accessors!(GoalFlags::MOVE | GoalFlags::JUMP);
    fn can_use(&self, _: &GoalContext<'_>) -> bool {
        self.armed
    }
    fn can_continue_to_use(&self, _: &GoalContext<'_>) -> bool {
        self.leap_ticks > 0
    }
    fn start(&mut self, _: &GoalContext<'_>) {
        self.leap_ticks = LEAP_IMPULSE_TICKS;
        self.armed = false;
    }
    fn tick(&mut self, ctx: &GoalContext<'_>) -> Option<GoalOutput> {
        let target = ctx.target?;
        let first = self.leap_ticks == LEAP_IMPULSE_TICKS;
        self.leap_ticks = self.leap_ticks.saturating_sub(1);
        let leap = spider_leap_velocity(
            [ctx.position.0, ctx.position.1, ctx.position.2],
            [target.position.0, target.position.1, target.position.2],
        );
        Some(GoalOutput {
            move_impulse: Some((leap[0], leap[2])),
            look_target: Some(target.position),
            jump: first,
            ..GoalOutput::default()
        })
    }
    fn stop(&mut self, _: &GoalContext<'_>) {
        self.leap_ticks = 0;
    }
    fn idle_tick(&mut self, ctx: &GoalContext<'_>) {
        self.armed = self
            .horizontal_distance_sq(ctx)
            .is_some_and(|distance_sq| {
                (4.0..=16.0).contains(&distance_sq)
                    && goal_seed_draw(&mut self.seed).is_multiple_of(5)
            });
    }
}

/// Vanilla `Spider.SpiderAttackGoal` (1.21.1): a `MeleeAttackGoal` that also
/// drops aggression in bright ambient light — spiders are daylight-neutral
/// unless already provoked (`recently_hurt` covers vanilla's `lastHurtByMob`
/// path since this engine has no attack attribution). Open-sky daylight
/// approximates vanilla's `brightness >= 0.5` (sky light >= 12).
#[derive(Debug)]
pub(crate) struct SpiderAttackGoal {
    inner: MeleeAttackGoal,
    daytime: RuleProbe,
}

impl SpiderAttackGoal {
    pub fn new(priority: i32) -> Self {
        Self {
            inner: MeleeAttackGoal::new(priority),
            daytime: RuleProbe(overworld_is_day),
        }
    }
}

impl Goal for SpiderAttackGoal {
    delegated_accessors!(inner);
    delegated_lifecycle!();
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool {
        self.inner.can_use(ctx)
            && !(mob_in_open_daylight(ctx, self.daytime) && !ctx.recently_hurt)
    }
    fn tick(&mut self, ctx: &GoalContext<'_>) -> Option<GoalOutput> {
        self.inner.tick(ctx)
    }
    fn stop_output(&self) -> Option<GoalOutput> {
        self.inner.stop_output()
    }
}

/// Vanilla `Zombie.ZombieAttackGoal` (1.21.1): a `MeleeAttackGoal` whose mob
/// raises its arms while the goal runs (`setAggressive(true)` on start,
/// lowered on stop).
#[derive(Debug)]
pub(crate) struct ZombieAttackGoal {
    inner: MeleeAttackGoal,
}

impl ZombieAttackGoal {
    pub fn new(priority: i32) -> Self {
        Self {
            inner: MeleeAttackGoal::new(priority),
        }
    }
}

impl Goal for ZombieAttackGoal {
    delegated_accessors!(inner);
    delegated_lifecycle!();
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool {
        self.inner.can_use(ctx)
    }
    fn tick(&mut self, ctx: &GoalContext<'_>) -> Option<GoalOutput> {
        let mut output = self.inner.tick(ctx)?;
        output.arms_raised = Some(true);
        Some(output)
    }
    fn stop_output(&self) -> Option<GoalOutput> {
        Some(GoalOutput {
            arms_raised: Some(false),
            ..GoalOutput::default()
        })
    }
}

/// Vanilla `BreakDoorGoal` (1.21.1): a `DoorInteractGoal` gated by
/// `mobGriefing` and a difficulty predicate (the zombie family binds
/// `difficulty == HARD`). While a closed wooden door sits within the mob's
/// reach it ticks `doorBreakTime`; every 24 ticks is one of the ten crack
/// stages broadcast via `destroyBlockProgress`, and at 240 ticks the door
/// block breaks. `canContinueToUse` resets progress when the door opens or
/// vanishes, matching vanilla.
#[derive(Debug)]
pub(crate) struct BreakDoorGoal {
    pub priority: i32,
    mob_griefing: RuleProbe,
    difficulty_test: RuleProbe,
    door: Option<(i32, i32, i32)>,
    break_time: u32,
}

/// Vanilla `destroyBlockTime` — the hard-difficulty door break is 240 ticks.
const DOOR_BREAK_TICKS: u32 = 240;

impl BreakDoorGoal {
    pub fn new(priority: i32) -> Self {
        Self {
            priority,
            mob_griefing: RuleProbe(mob_griefing_enabled),
            difficulty_test: RuleProbe(difficulty_is_hard),
            door: None,
            break_time: 0,
        }
    }

    /// A closed wooden door within arm's reach — the cells vanilla's
    /// `DoorInteractGoal` sees from the mob's navigation node.
    fn find_door(&self, ctx: &GoalContext<'_>) -> Option<(i32, i32, i32)> {
        nearest_cell(ctx, 1, 1, |world, (x, y, z)| {
            world.path_type(x, y, z) == BlockPathType::DoorWoodClosed
        })
    }
}

impl Goal for BreakDoorGoal {
    goal_accessors!(GoalFlags::MOVE | GoalFlags::JUMP);
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool {
        self.mob_griefing.check()
            && self.difficulty_test.check()
            && self.find_door(ctx).is_some()
    }
    fn can_continue_to_use(&self, ctx: &GoalContext<'_>) -> bool {
        self.mob_griefing.check()
            && self.difficulty_test.check()
            && self.break_time < DOOR_BREAK_TICKS
            && self.door.is_some_and(|(x, y, z)| {
                ctx.world.is_some_and(|world| {
                    world.path_type(x, y, z) == BlockPathType::DoorWoodClosed
                })
            })
    }
    fn start(&mut self, ctx: &GoalContext<'_>) {
        self.door = self.find_door(ctx);
        self.break_time = 0;
    }
    fn tick(&mut self, _: &GoalContext<'_>) -> Option<GoalOutput> {
        self.break_time = self.break_time.saturating_add(1);
        let door = self.door?;
        if self.break_time >= DOOR_BREAK_TICKS {
            return Some(GoalOutput {
                destroy_block: Some(door),
                ..GoalOutput::default()
            });
        }
        // Vanilla broadcasts the crack stage `doorBreakTime / 240 * 10`,
        // clamped to the 0..=9 animation stages.
        let stage = (self.break_time / 24).min(9) as u8;
        Some(GoalOutput {
            door_break_progress: Some((door, stage)),
            look_target: Some((
                f64::from(door.0) + 0.5,
                f64::from(door.1) + 0.5,
                f64::from(door.2) + 0.5,
            )),
            ..GoalOutput::default()
        })
    }
    fn stop(&mut self, _: &GoalContext<'_>) {
        self.door = None;
        self.break_time = 0;
    }
}

/// Vanilla `Zombie.ZombieAttackTurtleEggGoal` (1.21.1), a `RemoveBlockGoal` —
/// every `20 + nextInt(20)` idle ticks it scans 24 wide x 3 tall for a
/// `turtle_egg` block, then paths onto it and jumps on the spot every 20
/// ticks until `ticksReachedTarget > 60` removes the block.
#[derive(Debug)]
pub(crate) struct ZombieAttackTurtleEggGoal {
    pub priority: i32,
    mob_griefing: RuleProbe,
    seed: u64,
    next_scan: u32,
    egg: Option<(i32, i32, i32)>,
    stomp_ticks: u32,
    /// Vanilla `ticksToReachGoal`; a stale egg target expires after 600.
    ticks_left: u32,
}

impl ZombieAttackTurtleEggGoal {
    pub fn new(priority: i32, seed: u64) -> Self {
        Self {
            priority,
            mob_griefing: RuleProbe(mob_griefing_enabled),
            seed,
            next_scan: 0,
            egg: None,
            stomp_ticks: 0,
            ticks_left: 600,
        }
    }

    /// `findNearestBlock` — the closest turtle-egg cell in the vanilla
    /// MoveToBlockGoal search volume (24 horizontal, 3 vertical).
    fn find_egg(&self, ctx: &GoalContext<'_>) -> Option<(i32, i32, i32)> {
        nearest_cell(ctx, 12, 1, |world, (x, y, z)| {
            world.block_name(x, y, z) == "minecraft:turtle_egg"
        })
    }

    /// `isReachedTarget`: the mob stands on the egg — within vanilla's 1.14
    /// accepted distance of the block above it.
    fn reached_egg(&self, ctx: &GoalContext<'_>) -> bool {
        self.egg
            .is_some_and(|cell| cell_within(ctx, cell, 1.0, 1.14_f64.powi(2)))
    }
}

impl Goal for ZombieAttackTurtleEggGoal {
    goal_accessors!(GoalFlags::MOVE | GoalFlags::JUMP);
    fn can_use(&self, _: &GoalContext<'_>) -> bool {
        self.mob_griefing.check() && self.egg.is_some()
    }
    fn can_continue_to_use(&self, ctx: &GoalContext<'_>) -> bool {
        self.mob_griefing.check()
            && self.ticks_left > 0
            && self.egg.is_some_and(|(x, y, z)| {
                ctx.world.is_some_and(|world| {
                    world.block_name(x, y, z) == "minecraft:turtle_egg"
                })
            })
    }
    fn start(&mut self, _: &GoalContext<'_>) {
        self.stomp_ticks = 0;
        self.ticks_left = 600;
    }
    fn tick(&mut self, ctx: &GoalContext<'_>) -> Option<GoalOutput> {
        self.ticks_left = self.ticks_left.saturating_sub(1);
        let (x, y, z) = self.egg?;
        if !self.reached_egg(ctx) {
            // `getMoveToTarget` is the block above the egg — the mob stands
            // on the egg to stomp it.
            return Some(GoalOutput {
                move_target: Some((
                    f64::from(x) + 0.5,
                    f64::from(y) + 1.0,
                    f64::from(z) + 0.5,
                )),
                ..GoalOutput::default()
            });
        }
        self.stomp_ticks = self.stomp_ticks.saturating_add(1);
        if self.stomp_ticks > 60 {
            return Some(GoalOutput {
                destroy_block: Some((x, y, z)),
                ..GoalOutput::default()
            });
        }
        Some(GoalOutput {
            // Every 20 ticks the mob hops onto the egg again.
            jump: self.stomp_ticks >= 20 && self.stomp_ticks.is_multiple_of(20),
            look_target: Some((f64::from(x) + 0.5, f64::from(y), f64::from(z) + 0.5)),
            ..GoalOutput::default()
        })
    }
    fn stop(&mut self, _: &GoalContext<'_>) {
        self.egg = None;
        self.stomp_ticks = 0;
    }
    fn idle_tick(&mut self, ctx: &GoalContext<'_>) {
        if self.next_scan > 0 {
            self.next_scan -= 1;
            return;
        }
        self.next_scan = 20 + (goal_seed_draw(&mut self.seed) % 20) as u32;
        self.egg = self.find_egg(ctx);
    }
}

/// Vanilla `RestrictSunGoal` (1.21.1): while the world is day it sets the
/// ground navigation's `avoidSun` flag — a pure restriction marker with no
/// vanilla goal flags of its own.
#[derive(Debug)]
pub(crate) struct RestrictSunGoal {
    pub priority: i32,
    daytime: RuleProbe,
}

impl RestrictSunGoal {
    pub fn new(priority: i32) -> Self {
        Self {
            priority,
            daytime: RuleProbe(overworld_is_day),
        }
    }
}

impl Goal for RestrictSunGoal {
    goal_accessors!(0);
    fn can_use(&self, _: &GoalContext<'_>) -> bool {
        self.daytime.check()
    }
    fn start(&mut self, _: &GoalContext<'_>) {}
    fn tick(&mut self, _: &GoalContext<'_>) -> Option<GoalOutput> {
        Some(GoalOutput {
            restrict_sun: Some(true),
            ..GoalOutput::default()
        })
    }
    fn stop(&mut self, _: &GoalContext<'_>) {}
    fn stop_output(&self) -> Option<GoalOutput> {
        Some(GoalOutput {
            restrict_sun: Some(false),
            ..GoalOutput::default()
        })
    }
}

/// Vanilla `FleeSunGoal` (1.21.1): a burning, helmetless undead under open
/// sky picks a nearby shaded standable cell and navigates to it. Our mobs
/// carry no helmet slot, so the vanilla helmet clause is always satisfied.
#[derive(Debug)]
pub(crate) struct FleeSunGoal {
    pub priority: i32,
    daytime: RuleProbe,
    hide_pos: Option<(i32, i32, i32)>,
}

impl FleeSunGoal {
    pub fn new(priority: i32) -> Self {
        Self {
            priority,
            daytime: RuleProbe(overworld_is_day),
            hide_pos: None,
        }
    }

    /// `setHidePlacePosition` — the nearest standable cell within 11x5x11
    /// that daylight cannot reach.
    fn find_shade(&self, ctx: &GoalContext<'_>) -> Option<(i32, i32, i32)> {
        let evaluator = WalkNodeEvaluator {
            mob_height: ctx.height,
            ..WalkNodeEvaluator::default()
        };
        nearest_cell(ctx, 5, 2, |world, pos| {
            !world.sees_sky(pos.0, pos.1, pos.2) && evaluator.standable(world, pos)
        })
    }
}

impl Goal for FleeSunGoal {
    goal_accessors!(GoalFlags::MOVE);
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool {
        self.daytime.check()
            && ctx.on_fire
            && mob_in_open_daylight(ctx, self.daytime)
            && self.find_shade(ctx).is_some()
    }
    fn can_continue_to_use(&self, ctx: &GoalContext<'_>) -> bool {
        self.daytime.check()
            && self
                .hide_pos
                .is_some_and(|cell| !cell_within(ctx, cell, 0.0, 0.25))
    }
    fn start(&mut self, ctx: &GoalContext<'_>) {
        self.hide_pos = self.find_shade(ctx);
    }
    fn tick(&mut self, _: &GoalContext<'_>) -> Option<GoalOutput> {
        let (x, y, z) = self.hide_pos?;
        Some(GoalOutput {
            move_target: Some((
                f64::from(x) + 0.5,
                f64::from(y),
                f64::from(z) + 0.5,
            )),
            ..GoalOutput::default()
        })
    }
    fn stop(&mut self, _: &GoalContext<'_>) {
        self.hide_pos = None;
    }
}

/// Vanilla `MoveThroughVillageGoal` (1.21.1): at night the mob wanders toward
/// village doorway cells. With no POI index the vanilla door-search is
/// approximated by a rescan (every `20 + nextInt(20)` ticks) for the nearest
/// door path type inside 32x5x32; door-breakers additionally accept closed
/// wooden doors, which `BreakDoorGoal` then owns.
#[derive(Debug)]
pub(crate) struct MoveThroughVillageGoal {
    pub priority: i32,
    daytime: RuleProbe,
    can_break_doors: bool,
    seed: u64,
    next_scan: u32,
    dest: Option<(i32, i32, i32)>,
}

impl MoveThroughVillageGoal {
    pub fn new(priority: i32, can_break_doors: bool, seed: u64) -> Self {
        Self {
            priority,
            daytime: RuleProbe(overworld_is_day),
            can_break_doors,
            seed,
            next_scan: 0,
            dest: None,
        }
    }

    fn find_doorway(&self, ctx: &GoalContext<'_>) -> Option<(i32, i32, i32)> {
        nearest_cell(ctx, 16, 2, |world, (x, y, z)| {
            match world.path_type(x, y, z) {
                BlockPathType::DoorOpen => true,
                #[cfg(test)]
                BlockPathType::WalkableDoor => true,
                BlockPathType::DoorWoodClosed => self.can_break_doors,
                _ => false,
            }
        })
    }
}

impl Goal for MoveThroughVillageGoal {
    goal_accessors!(GoalFlags::MOVE);
    fn can_use(&self, _: &GoalContext<'_>) -> bool {
        !self.daytime.check() && self.dest.is_some()
    }
    fn can_continue_to_use(&self, ctx: &GoalContext<'_>) -> bool {
        !self.daytime.check()
            && self
                .dest
                .is_some_and(|cell| !cell_within(ctx, cell, 0.0, 1.0))
    }
    fn start(&mut self, _: &GoalContext<'_>) {}
    fn tick(&mut self, _: &GoalContext<'_>) -> Option<GoalOutput> {
        let (x, y, z) = self.dest?;
        Some(GoalOutput {
            move_target: Some((
                f64::from(x) + 0.5,
                f64::from(y),
                f64::from(z) + 0.5,
            )),
            ..GoalOutput::default()
        })
    }
    fn stop(&mut self, _: &GoalContext<'_>) {
        self.dest = None;
    }
    fn idle_tick(&mut self, ctx: &GoalContext<'_>) {
        if self.next_scan > 0 {
            self.next_scan -= 1;
            return;
        }
        self.next_scan = 20 + (goal_seed_draw(&mut self.seed) % 20) as u32;
        self.dest = (!self.daytime.check()).then(|| self.find_doorway(ctx)).flatten();
    }
}

/// Vanilla `RangedBowAttackGoal` (1.21.1): navigates to the target until it
/// has been seen for 20 consecutive ticks inside the 15-block attack radius,
/// then strafes (`strafingClockwise`/`strafingBackwards` flipping on a
/// 20-tick cadence) while the bow draws. Once `attackTime` runs out the mob
/// pulls the bow and releases after 20 drawn ticks; each release resets the
/// interval (20 ticks hard / 40 ticks normal for skeletons).
#[derive(Debug)]
pub(crate) struct RangedBowAttackGoal {
    pub priority: i32,
    /// Mob eye height; the arrow spawns at `getEyeY() - 0.1`.
    eye_height: f64,
    /// Tipped-arrow effect (`stray` slowness, `bogged` poison); `None` for a
    /// plain skeleton arrow.
    tipped: Option<&'static str>,
    /// Hard-difficulty probe; skeletons pass `difficulty == HARD`.
    hard: RuleProbe,
    seed: u64,
    /// Vanilla `seeTime`: positive streak with line of sight, negative streak
    /// without.
    see_time: i32,
    /// Vanilla `attackTime` countdown between draws.
    attack_time: i32,
    /// Ticks the bow has been held drawn; releases at `BOW_DRAW_TICKS`.
    bow_pull: u32,
    strafe_time: i32,
    strafe_clockwise: bool,
    strafe_backwards: bool,
}

const BOW_DRAW_TICKS: u32 = 20;
const BOW_ATTACK_RADIUS: f64 = 15.0;

impl RangedBowAttackGoal {
    pub fn new(priority: i32, eye_height: f64, tipped: Option<&'static str>, seed: u64) -> Self {
        Self {
            priority,
            eye_height,
            tipped,
            hard: RuleProbe(difficulty_is_hard),
            seed,
            see_time: 0,
            attack_time: 0,
            bow_pull: 0,
            strafe_time: -1,
            strafe_clockwise: false,
            strafe_backwards: false,
        }
    }

    fn attack_interval(&self) -> i32 {
        i32::from(skeleton_bow_charge_interval(self.hard.check()))
    }
}

impl Goal for RangedBowAttackGoal {
    goal_accessors!(GoalFlags::MOVE | GoalFlags::LOOK);
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool {
        ctx.target.is_some()
    }
    fn start(&mut self, _: &GoalContext<'_>) {}
    fn tick(&mut self, ctx: &GoalContext<'_>) -> Option<GoalOutput> {
        let target = ctx.target?;
        let distance_sq = entity_distance_sq(ctx.position, target.position);
        let distance = distance_sq.sqrt();
        if target.line_of_sight {
            self.see_time = self.see_time.saturating_add(1);
        } else {
            self.see_time = self.see_time.saturating_sub(1);
        }
        let radius_sq = BOW_ATTACK_RADIUS * BOW_ATTACK_RADIUS;
        // Vanilla stops navigating once it has held sight for 20 ticks
        // inside the radius and otherwise paths to the target.
        let stationary = distance_sq <= radius_sq && self.see_time >= 20;
        let move_target = (!stationary).then_some(target.position);
        if stationary {
            self.strafe_time += 1;
        } else {
            self.strafe_time = -1;
        }
        if self.strafe_time >= 20 {
            if goal_seed_draw(&mut self.seed) % 10 < 3 {
                self.strafe_clockwise = !self.strafe_clockwise;
            }
            if goal_seed_draw(&mut self.seed) % 10 < 3 {
                self.strafe_backwards = !self.strafe_backwards;
            }
            self.strafe_time = 0;
        }
        let mut move_impulse = None;
        if self.strafe_time > -1 {
            // Vanilla's move-control strafe bands: backwards inside a quarter
            // of the radius square, never backwards past three quarters.
            if distance_sq > radius_sq * 0.75 {
                self.strafe_backwards = false;
            } else if distance_sq < radius_sq * 0.25 {
                self.strafe_backwards = true;
            }
            let strafe = skeleton_strafe_velocity(
                distance,
                self.strafe_clockwise,
                !self.strafe_backwards,
            );
            move_impulse = Some((strafe[0], strafe[2]));
        }
        let mut ranged_attack = None;
        let mut attack_target = None;
        if self.bow_pull > 0 {
            if !target.line_of_sight && self.see_time < -60 {
                // `stopUsingItem` without firing once sight has been lost
                // for 60 ticks.
                self.bow_pull = 0;
            } else if target.line_of_sight {
                self.bow_pull += 1;
                if self.bow_pull >= BOW_DRAW_TICKS {
                    let third_height = entity_spec_by_id(target.etype)
                        .map_or(0.6, |spec| f64::from(spec.height) / 3.0);
                    let velocity = skeleton_arrow_velocity(
                        [ctx.position.0, ctx.position.1, ctx.position.2],
                        [target.position.0, target.position.1, target.position.2],
                        third_height,
                        self.eye_height,
                    );
                    ranged_attack = Some(RangedProjectileAction::Arrow {
                        target_eid: target.entity_id,
                        target_pos: target.position,
                        velocity: (velocity[0], velocity[1], velocity[2]),
                        tipped: self.tipped,
                    });
                    attack_target = Some(target.entity_id);
                    self.attack_time = self.attack_interval();
                    self.bow_pull = 0;
                }
            }
        } else {
            self.attack_time -= 1;
            if self.attack_time <= 0 && self.see_time >= -60 {
                // `startUsingItem` — the bow begins its draw.
                self.bow_pull = 1;
            }
        }
        Some(GoalOutput {
            move_target,
            move_impulse,
            look_target: Some(target.position),
            ranged_attack,
            attack_target,
            ..GoalOutput::default()
        })
    }
    fn stop(&mut self, _: &GoalContext<'_>) {
        self.see_time = 0;
        self.attack_time = 0;
        self.bow_pull = 0;
        self.strafe_time = -1;
    }
}

#[derive(Debug)]
pub(crate) struct PanicGoal {
    pub priority: i32,
    destination: Option<(f64, f64, f64)>,
}

impl PanicGoal {
    pub fn new(priority: i32) -> Self {
        Self {
            priority,
            destination: None,
        }
    }

    fn flee_position(&self, ctx: &GoalContext<'_>) -> (f64, f64, f64) {
        if let Some(world) = ctx.world {
            let center = (
                ctx.position.0.floor() as i32,
                ctx.position.1.floor() as i32,
                ctx.position.2.floor() as i32,
            );
            let evaluator = WalkNodeEvaluator {
                mob_height: ctx.height,
                ..WalkNodeEvaluator::default()
            };
            let mut best: Option<(f64, (i32, i32, i32))> = None;
            for dx in -8..=8_i32 {
                for dz in -8..=8_i32 {
                    for dy in -1..=1_i32 {
                        let Some(x) = center.0.checked_add(dx) else {
                            continue;
                        };
                        let Some(y) = center.1.checked_add(dy) else {
                            continue;
                        };
                        let Some(z) = center.2.checked_add(dz) else {
                            continue;
                        };
                        let pos = (x, y, z);
                        if !evaluator.standable(world, pos) {
                            continue;
                        }
                        let ty = world.path_type(x, y, z);
                        if default_malus(ty) > 0.0 && ty != BlockPathType::Water {
                            continue;
                        }
                        let target_pos = (f64::from(x) + 0.5, f64::from(y), f64::from(z) + 0.5);
                        let distance = ctx.target.map_or_else(
                            || entity_distance_sq(target_pos, ctx.position),
                            |target| entity_distance_sq(target_pos, target.position),
                        );
                        let score = distance
                            + if ctx.on_fire && ty == BlockPathType::Water {
                                1000.0
                            } else {
                                0.0
                            };
                        if best.is_none_or(|(old, old_pos)| {
                            score > old || (score == old && pos < old_pos)
                        }) {
                            best = Some((score, pos));
                        }
                    }
                }
            }
            if let Some((_, (x, y, z))) = best {
                return (f64::from(x) + 0.5, f64::from(y), f64::from(z) + 0.5);
            }
        }
        let (dx, dz) = ctx.target.map_or((1.0, 0.0), |threat| {
            (
                ctx.position.0 - threat.position.0,
                ctx.position.2 - threat.position.2,
            )
        });
        let norm = dx.hypot(dz).max(1.0);
        (
            ctx.position.0 + 8.0 * dx / norm,
            ctx.position.1,
            ctx.position.2 + 8.0 * dz / norm,
        )
    }
}

impl Goal for PanicGoal {
    goal_accessors!(GoalFlags::MOVE);
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool {
        ctx.on_fire || ctx.recently_hurt
    }
    fn start(&mut self, ctx: &GoalContext<'_>) {
        self.destination = Some(self.flee_position(ctx));
    }
    fn tick(&mut self, _: &GoalContext<'_>) -> Option<GoalOutput> {
        Some(GoalOutput {
            move_target: self.destination,
            ..GoalOutput::default()
        })
    }
    fn stop(&mut self, _: &GoalContext<'_>) {
        self.destination = None;
    }
}

#[derive(Debug)]
pub(crate) struct TemptGoal {
    pub priority: i32,
    pub tempting_item: u32,
    pub calm_down_on_interrupt: bool,
    calm_down: u32,
}

impl TemptGoal {
    pub fn new(priority: i32, tempting_item: u32) -> Self {
        Self {
            priority,
            tempting_item,
            calm_down_on_interrupt: true,
            calm_down: 0,
        }
    }
}

impl Goal for TemptGoal {
    goal_accessors!(GoalFlags::MOVE | GoalFlags::LOOK);
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool {
        self.calm_down == 0
            && ctx.nearest_player.is_some_and(|player| {
                player.held_item == Some(self.tempting_item)
                    && entity_distance_sq(ctx.position, player.position) < 100.0
            })
    }
    fn start(&mut self, _: &GoalContext<'_>) {}
    fn tick(&mut self, ctx: &GoalContext<'_>) -> Option<GoalOutput> {
        let player = ctx.nearest_player?;
        Some(GoalOutput {
            move_target: (entity_distance_sq(ctx.position, player.position) > 6.25)
                .then_some(player.position),
            look_target: Some(player.position),
            ..GoalOutput::default()
        })
    }
    fn stop(&mut self, _: &GoalContext<'_>) {
        if self.calm_down_on_interrupt {
            self.calm_down = 100;
        }
    }
    fn idle_tick(&mut self, _: &GoalContext<'_>) {
        self.calm_down = self.calm_down.saturating_sub(1);
    }
}

#[derive(Debug)]
pub(crate) struct BreedGoal {
    pub priority: i32,
    nearby_ticks: u32,
    completed: bool,
}

impl BreedGoal {
    pub fn new(priority: i32) -> Self {
        Self {
            priority,
            nearby_ticks: 0,
            completed: false,
        }
    }

    fn valid_mate(ctx: &GoalContext<'_>) -> Option<AiEntityPerception> {
        ctx.mate.filter(|mate| {
            ctx.in_love
                && mate.in_love
                && mate.compatible
                && entity_distance_sq(ctx.position, mate.position) <= 64.0
        })
    }
}

impl Goal for BreedGoal {
    goal_accessors!(GoalFlags::MOVE | GoalFlags::LOOK);
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool {
        Self::valid_mate(ctx).is_some() && !self.completed
    }
    fn start(&mut self, _: &GoalContext<'_>) {
        self.nearby_ticks = 0;
    }
    fn tick(&mut self, ctx: &GoalContext<'_>) -> Option<GoalOutput> {
        let mate = Self::valid_mate(ctx)?;
        let nearby = entity_distance_sq(ctx.position, mate.position) <= 9.0;
        self.nearby_ticks = if nearby {
            self.nearby_ticks.saturating_add(1)
        } else {
            0
        };
        let breed_target = (self.nearby_ticks >= 60).then_some(mate.entity_id);
        if breed_target.is_some() {
            self.completed = true;
        }
        Some(GoalOutput {
            move_target: Some(mate.position),
            look_target: Some(mate.position),
            breed_target,
            ..GoalOutput::default()
        })
    }
    fn stop(&mut self, _: &GoalContext<'_>) {
        self.nearby_ticks = 0;
    }
    fn idle_tick(&mut self, ctx: &GoalContext<'_>) {
        if !ctx.in_love {
            self.completed = false;
        }
    }
}

#[derive(Debug)]
pub(crate) struct WaterAvoidingRandomStrollGoal {
    pub priority: i32,
    seed: u64,
    destination: Option<(f64, f64, f64)>,
}

impl WaterAvoidingRandomStrollGoal {
    pub fn new(priority: i32, seed: u64) -> Self {
        Self {
            priority,
            seed,
            destination: None,
        }
    }

    fn next_random(&mut self) -> u64 {
        goal_seed_draw(&mut self.seed)
    }

    fn select_destination(&mut self, ctx: &GoalContext<'_>) -> Option<(f64, f64, f64)> {
        let world = ctx.world?;
        let evaluator = WalkNodeEvaluator {
            mob_height: ctx.height,
            ..WalkNodeEvaluator::default()
        };
        let (cx, cy, cz) = (
            ctx.position.0.floor() as i32,
            ctx.position.1.floor() as i32,
            ctx.position.2.floor() as i32,
        );
        let mut fallback = None;
        for _ in 0..32 {
            let dx = (self.next_random() % 21) as i32 - 10;
            let dz = (self.next_random() % 21) as i32 - 10;
            let dy = (self.next_random() % 5) as i32 - 2;
            let (Some(x), Some(y), Some(z)) =
                (cx.checked_add(dx), cy.checked_add(dy), cz.checked_add(dz))
            else {
                continue;
            };
            if !evaluator.standable(world, (x, y, z)) {
                continue;
            }
            let path_type = world.path_type(x, y, z);
            let point = (f64::from(x) + 0.5, f64::from(y), f64::from(z) + 0.5);
            if matches!(path_type, BlockPathType::Open | BlockPathType::Walkable) {
                return Some(point);
            }
            if path_type == BlockPathType::Water && fallback.is_none() {
                fallback = Some(point);
            }
        }
        fallback
    }
}

impl Goal for WaterAvoidingRandomStrollGoal {
    goal_accessors!(GoalFlags::MOVE);
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool {
        ctx.world.is_some() && self.destination.is_none()
    }
    fn can_continue_to_use(&self, ctx: &GoalContext<'_>) -> bool {
        self.destination
            .is_some_and(|destination| entity_distance_sq(ctx.position, destination) > 1.0)
    }
    fn start(&mut self, ctx: &GoalContext<'_>) {
        self.destination = self.select_destination(ctx);
    }
    fn tick(&mut self, _: &GoalContext<'_>) -> Option<GoalOutput> {
        Some(GoalOutput {
            move_target: self.destination,
            ..GoalOutput::default()
        })
    }
    fn stop(&mut self, _: &GoalContext<'_>) {
        self.destination = None;
    }
}

#[derive(Debug)]
pub(crate) struct LookAtPlayerGoal {
    pub priority: i32,
    pub look_distance: f64,
}

impl Goal for LookAtPlayerGoal {
    goal_accessors!(GoalFlags::LOOK);
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool {
        ctx.nearest_player.is_some_and(|player| {
            entity_distance_sq(ctx.position, player.position)
                <= self.look_distance * self.look_distance
        })
    }
    fn start(&mut self, _: &GoalContext<'_>) {}
    fn tick(&mut self, ctx: &GoalContext<'_>) -> Option<GoalOutput> {
        Some(GoalOutput {
            look_target: Some(ctx.nearest_player?.position),
            ..GoalOutput::default()
        })
    }
    fn stop(&mut self, _: &GoalContext<'_>) {}
}

/// Vanilla `AvoidEntityGoal` constructor arguments: which entity types to
/// flee, the trigger radius, and the walk/sprint speed modifiers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct AvoidEntitySpec {
    /// Protocol entity type ids the mob flees from.
    pub flee_etypes: &'static [i64],
    /// Rolled-up (`scared`) entities do not count; vanilla spiders only flee
    /// armadillos that are not rolled up.
    pub require_unscared: bool,
    /// Flee trigger radius in blocks (vanilla `maxDist`, 6.0 here).
    pub max_dist: f32,
    /// Navigation speed multiplier while the threat is past the sprint band.
    pub walk_speed: f32,
    /// Navigation speed multiplier while the threat is closer than 7 blocks
    /// (vanilla hard-codes the `distanceToSqr < 49` sprint check).
    pub sprint_speed: f32,
}

/// Vanilla `AvoidEntityGoal` (1.21.1): flee the nearest matching mob inside
/// `max_dist`. The goal keeps a straight-line flee destination up to date;
/// the caller applies `speed_modifier` to the waypoint stride.
#[derive(Debug)]
pub(crate) struct AvoidEntityGoal {
    pub priority: i32,
    pub spec: AvoidEntitySpec,
    threat: Option<NearbyMob>,
}

impl AvoidEntityGoal {
    pub fn new(priority: i32, spec: AvoidEntitySpec) -> Self {
        Self {
            priority,
            spec,
            threat: None,
        }
    }
}

impl Goal for AvoidEntityGoal {
    goal_accessors!(GoalFlags::MOVE);
    fn perceive(&mut self, position: (f64, f64, f64), perception: &MobPerception) {
        self.threat = perception.nearest(position, f64::from(self.spec.max_dist), |mob| {
            self.spec.flee_etypes.contains(&mob.etype)
                && !(self.spec.require_unscared && mob.scared)
        });
    }
    fn can_use(&self, _: &GoalContext<'_>) -> bool {
        self.threat.is_some()
    }
    fn start(&mut self, _: &GoalContext<'_>) {}
    fn tick(&mut self, ctx: &GoalContext<'_>) -> Option<GoalOutput> {
        let threat = self.threat?;
        let (dx, dz) = (
            ctx.position.0 - threat.position.0,
            ctx.position.2 - threat.position.2,
        );
        let norm = dx.hypot(dz);
        let (fx, fz) = if norm > 1.0e-6 {
            (dx / norm, dz / norm)
        } else {
            (1.0, 0.0)
        };
        let reach = f64::from(self.spec.max_dist);
        // Vanilla switches to the sprint modifier inside 7 blocks of the
        // threat and walks the flee path otherwise.
        let speed_modifier = if entity_distance_sq(ctx.position, threat.position) < 49.0 {
            self.spec.sprint_speed
        } else {
            self.spec.walk_speed
        };
        Some(GoalOutput {
            move_target: Some((
                ctx.position.0 + fx * reach,
                ctx.position.1,
                ctx.position.2 + fz * reach,
            )),
            speed_modifier: Some(f64::from(speed_modifier)),
            ..GoalOutput::default()
        })
    }
    fn stop(&mut self, _: &GoalContext<'_>) {}
}

/// Vanilla `RangedAttackGoal` as registered on `SnowGolem` (1.21.1):
/// `new RangedAttackGoal(this, 1.25, 20, 10.0)`. The goal stops navigating
/// once it has held line of sight inside the attack radius and launches a
/// `Snowball` projectile on the 20-tick (1 second) interval.
#[derive(Debug)]
pub(crate) struct RangedSnowballGoal {
    pub priority: i32,
    /// Vanilla `attackRadius` in blocks.
    pub attack_radius: f32,
    /// Vanilla `attackInterval` in ticks (snow golem: 20 == 1.0 s).
    pub attack_interval: u32,
    /// Vanilla `seeTime`: consecutive ticks of target line of sight.
    see_time: u32,
    /// Vanilla `attackTime` countdown; `-1` arms the initial interval.
    attack_time: i32,
}

impl RangedSnowballGoal {
    pub fn new(priority: i32) -> Self {
        Self {
            priority,
            attack_radius: 10.0,
            attack_interval: 20,
            see_time: 0,
            attack_time: -1,
        }
    }
}

impl Goal for RangedSnowballGoal {
    goal_accessors!(GoalFlags::MOVE | GoalFlags::LOOK);
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool {
        ctx.target.is_some()
    }
    fn start(&mut self, _: &GoalContext<'_>) {}
    fn tick(&mut self, ctx: &GoalContext<'_>) -> Option<GoalOutput> {
        let target = ctx.target?;
        let distance_sq = entity_distance_sq(ctx.position, target.position);
        if target.line_of_sight {
            self.see_time = self.see_time.saturating_add(1);
        } else {
            self.see_time = 0;
        }
        let radius_sq = f64::from(self.attack_radius) * f64::from(self.attack_radius);
        let within = distance_sq <= radius_sq;
        // Vanilla navigates to the target until it is in range and has been
        // seen for 5 consecutive ticks, then holds position.
        let move_target = (!within || self.see_time < 5).then_some(target.position);
        self.attack_time -= 1;
        let mut ranged_attack = None;
        if self.attack_time == 0 {
            if target.line_of_sight && within {
                // Target eye line minus the 1.1 projectile spawn offset; the
                // +0.2/block arc compensation lives in the velocity helper.
                let eye_height =
                    entity_spec_by_id(target.etype).map_or(1.62, |spec| f64::from(spec.eye_height));
                let velocity = snow_golem_snowball_velocity(
                    [ctx.position.0, ctx.position.1, ctx.position.2],
                    [target.position.0, target.position.1, target.position.2],
                    eye_height,
                );
                ranged_attack = Some(RangedProjectileAction::Snowball {
                    target_eid: target.entity_id,
                    target_pos: target.position,
                    velocity: (velocity[0], velocity[1], velocity[2]),
                });
                // Vanilla floors `distance/radius * (max - min) + min`; the
                // snow golem registers min == max == `attack_interval`.
                self.attack_time = i32::try_from(self.attack_interval).unwrap_or(i32::MAX);
            }
        } else if self.attack_time < 0 {
            self.attack_time = i32::try_from(self.attack_interval).unwrap_or(i32::MAX);
        }
        Some(GoalOutput {
            move_target,
            look_target: Some(target.position),
            ranged_attack,
            ..GoalOutput::default()
        })
    }
    fn stop(&mut self, _: &GoalContext<'_>) {
        self.see_time = 0;
        self.attack_time = -1;
    }
}

/// How a `NearestAttackableMobGoal` accepts nearby mobs (1.21.1 predicates).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum MobPreySelector {
    /// `entity instanceof Enemy` — vanilla's hostile-mob marker interface,
    /// which maps onto the registry's `Monster` category.
    HostileMobs,
    /// `instanceof Enemy && !instanceof Excluded` (iron golems skip creepers).
    HostileExcept(&'static [i64]),
    /// An explicit list of protocol entity type ids.
    Etypes(&'static [i64]),
}

/// Vanilla `NearestAttackableTargetGoal` parameters for mob-vs-mob prey.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MobTargetSpec {
    pub prey: MobPreySelector,
    /// Acquisition radius in blocks (vanilla `follow_range` — 16.0 default).
    pub max_dist: f64,
    /// Vanilla `mustSee`: require line of sight to accept the candidate.
    pub must_see: bool,
    /// Only babies qualify (foxes only take turtles while they are babies).
    pub baby_only: bool,
}

fn is_enemy_etype(etype: i64) -> bool {
    entity_spec_by_id(etype).is_some_and(|spec| spec.category == EntityMobCategory::Monster)
}

/// Vanilla `NearestAttackableTargetGoal` for mob prey: scans `MobPerception`
/// for the nearest acceptable mob inside `max_dist` and publishes it on the
/// `mob_target` channel every tick while running.
#[derive(Debug)]
pub(crate) struct NearestAttackableMobGoal {
    pub priority: i32,
    pub spec: MobTargetSpec,
    candidate: Option<NearbyMob>,
}

impl NearestAttackableMobGoal {
    pub fn new(priority: i32, spec: MobTargetSpec) -> Self {
        Self {
            priority,
            spec,
            candidate: None,
        }
    }

    fn accepts(&self, mob: &NearbyMob) -> bool {
        let matches = match self.spec.prey {
            MobPreySelector::HostileMobs => is_enemy_etype(mob.etype),
            MobPreySelector::HostileExcept(excluded) => {
                is_enemy_etype(mob.etype) && !excluded.contains(&mob.etype)
            }
            MobPreySelector::Etypes(etypes) => etypes.contains(&mob.etype),
        };
        matches
            && (!self.spec.baby_only || mob.baby)
            && (!self.spec.must_see || mob.line_of_sight)
    }
}

impl Goal for NearestAttackableMobGoal {
    goal_accessors!(GoalFlags::TARGET);
    fn perceive(&mut self, position: (f64, f64, f64), perception: &MobPerception) {
        self.candidate = perception
            .nearest(position, self.spec.max_dist, |mob| self.accepts(mob));
    }
    fn can_use(&self, _: &GoalContext<'_>) -> bool {
        self.candidate.is_some()
    }
    fn start(&mut self, _: &GoalContext<'_>) {}
    fn tick(&mut self, _: &GoalContext<'_>) -> Option<GoalOutput> {
        Some(GoalOutput {
            mob_target: Some(self.candidate?.entity_id),
            ..GoalOutput::default()
        })
    }
    fn stop(&mut self, _: &GoalContext<'_>) {}
}

/// Vanilla `targetSelector` mob-vs-mob goals from each mob's
/// `registerTargetGoals` (1.21.1). Player-facing target goals stay with the
/// live player scan; only goals whose candidates come from `MobPerception`
/// are built here.
pub(crate) fn build_target_selector_for_etype(etype: i64) -> TargetSelector {
    use MobPreySelector::*;
    let mut selector = TargetSelector::default();
    let mut add = |priority: i32, prey, max_dist, must_see, baby_only| {
        selector.add_goal(Box::new(NearestAttackableMobGoal::new(
            priority,
            MobTargetSpec {
                prey,
                max_dist,
                must_see,
                baby_only,
            },
        )));
    };
    match etype {
        // SnowGolem: NearestAttackableTargetGoal<Mob> -> instanceof Enemy,
        // inside the snowball attack radius.
        96 => add(1, HostileMobs, 10.0, true, false),
        // IronGolem: Enemy && !(entity instanceof Creeper).
        57 => add(3, HostileExcept(&[23]), 16.0, false, false),
        // Untamed Wolf: Sheep/Rabbit/Fox prey plus the AbstractSkeleton family.
        122 => {
            add(4, Etypes(&[87, 84, 42]), 16.0, false, false);
            add(5, Etypes(&[91, 102, 11, 120]), 16.0, false, false);
        }
        // Fox: Chicken, Rabbit, small fish, and baby Turtle prey.
        42 => {
            add(4, Etypes(&[19]), 16.0, false, false);
            add(4, Etypes(&[84]), 16.0, false, false);
            add(4, Etypes(&[20, 86, 110]), 16.0, false, false);
            add(4, Etypes(&[111]), 16.0, false, true);
        }
        _ => {}
    }
    selector
}


/// Vanilla `DoorInteractGoal` as used by villagers and vindicators: a closed
/// wooden door in reach is a navigation obstacle the mob may open rather than
/// break. The caller owns the block-state write; the goal emits the cell and
/// steers through it.
#[derive(Debug)]
pub(crate) struct OpenDoorGoal {
    pub priority: i32,
    door: Option<(i32, i32, i32)>,
}

impl OpenDoorGoal {
    pub fn new(priority: i32) -> Self {
        Self { priority, door: None }
    }

    fn find_door(&self, ctx: &GoalContext<'_>) -> Option<(i32, i32, i32)> {
        nearest_cell(ctx, 1, 1, |world, (x, y, z)| {
            world.path_type(x, y, z) == BlockPathType::DoorWoodClosed
        })
    }
}

impl Goal for OpenDoorGoal {
    goal_accessors!(GoalFlags::MOVE | GoalFlags::LOOK);
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool { self.find_door(ctx).is_some() }
    fn can_continue_to_use(&self, ctx: &GoalContext<'_>) -> bool {
        self.door
            .is_some_and(|cell| cell_within(ctx, cell, 0.5, 9.0))
    }
    fn start(&mut self, ctx: &GoalContext<'_>) { self.door = self.find_door(ctx); }
    fn tick(&mut self, _: &GoalContext<'_>) -> Option<GoalOutput> {
        let (x, y, z) = self.door?;
        Some(GoalOutput {
            open_door: Some((x, y, z)),
            move_target: Some((f64::from(x) + 0.5, f64::from(y), f64::from(z) + 0.5)),
            look_target: Some((f64::from(x) + 0.5, f64::from(y) + 0.5, f64::from(z) + 0.5)),
            ..GoalOutput::default()
        })
    }
    fn stop(&mut self, _: &GoalContext<'_>) { self.door = None; }
}


/// A climb impulse for spiders and other `minecraft:climbable` users. Wall
/// climbing suppresses horizontal deflection and adds upward motion while the
/// mob is colliding horizontally; vines/ladders use the same vertical rule.
pub fn ai_climb_velocity(horizontal: (f64, f64), ascending: bool) -> (f64, f64, f64) {
    (
        horizontal.0 * 0.2,
        if ascending { 0.2 } else { 0.0 },
        horizontal.1 * 0.2,
    )
}

/// Amphibious/swimming navigation follows the target through the water column
/// instead of projecting the path onto the floor. Buoyancy and gravity remain
/// owned by entity physics.
pub fn ai_swim_velocity(
    position: (f64, f64, f64),
    target: (f64, f64, f64),
    speed: f64,
) -> (f64, f64, f64) {
    let delta = (target.0 - position.0, target.1 - position.1, target.2 - position.2);
    let len = (delta.0 * delta.0 + delta.1 * delta.1 + delta.2 * delta.2).sqrt();
    if len <= 1.0e-9 {
        (0.0, 0.0, 0.0)
    } else {
        (delta.0 / len * speed, delta.1 / len * speed, delta.2 / len * speed)
    }
}

/// 1.21.1 charged-creeper head drops. Ordinary explosions never set this loot
/// flag; the piglin head is part of the same vanilla rule in 1.21.1.
pub fn ai_charged_creeper_head_drop(
    victim: &str,
    killed_by_charged_creeper: bool,
) -> Option<&'static str> {
    if !killed_by_charged_creeper {
        return None;
    }
    match victim.strip_prefix("minecraft:").unwrap_or(victim) {
        "creeper" => Some("minecraft:creeper_head"),
        "piglin" => Some("minecraft:piglin_head"),
        "skeleton" => Some("minecraft:skeleton_skull"),
        "wither_skeleton" => Some("minecraft:wither_skeleton_skull"),
        "zombie" => Some("minecraft:zombie_head"),
        _ => None,
    }
}

/// Creeper fuse state. `swelling` is the goal's target-in-radius/LOS result.
/// Flint-and-steel ignition is irrevocable, and charged creepers double power.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AiCreeperFuse {
    pub fuse_ticks: i32,
    pub flint_ignited: bool,
    pub charged: bool,
}

impl AiCreeperFuse {
    pub const FUSE_TICKS: i32 = 30;

    pub const fn new(charged: bool) -> Self {
        Self { fuse_ticks: 0, flint_ignited: false, charged }
    }

    pub fn ignite_flint_and_steel(&mut self) {
        self.flint_ignited = true;
        self.fuse_ticks = self.fuse_ticks.max(0);
    }

    /// Returns explosion power on the detonating tick.
    pub fn tick(&mut self, swelling: bool) -> Option<f32> {
        if swelling || self.flint_ignited {
            self.fuse_ticks = self.fuse_ticks.saturating_add(1);
        } else {
            self.fuse_ticks = self.fuse_ticks.saturating_sub(1).max(0);
        }
        (self.fuse_ticks >= Self::FUSE_TICKS).then_some(if self.charged { 6.0 } else { 3.0 })
    }
}

/// Enderman block carrying is controlled by `minecraft:enderman_holdable` and
/// `mobGriefing`; placement additionally needs a replaceable destination over
/// a supporting full block. Registry lookups stay outside this pure rule.
pub fn ai_enderman_can_pick_block(mob_griefing: bool, block_is_enderman_holdable: bool) -> bool {
    mob_griefing && block_is_enderman_holdable
}

pub fn ai_enderman_can_place_block(
    mob_griefing: bool,
    carried_is_enderman_holdable: bool,
    destination_replaceable: bool,
    support_is_full_block: bool,
) -> bool {
    mob_griefing && carried_is_enderman_holdable && destination_replaceable && support_is_full_block
}

/// Vanilla projectile inaccuracy `14 - difficulty * 4`: Easy 10, Normal 6,
/// Hard 2. Peaceful never reaches the attack.
pub fn ai_projectile_inaccuracy(difficulty: u8) -> i32 {
    14 - i32::from(difficulty.min(3)) * 4
}

/// Pillager/crossbow charge state. `RangedCrossbowAttackGoal` charges the
/// crossbow for its 25-tick use duration and fires as soon as the charge
/// completes; the vanilla 20..40 tick random range is a pathfinding delay,
/// not a post-shot attack cooldown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AiCrossbowAttack {
    charge_ticks: u8,
}

impl Default for AiCrossbowAttack {
    fn default() -> Self {
        Self::new()
    }
}

impl AiCrossbowAttack {
    pub const CHARGE_TICKS: u8 = 25;

    pub const fn new() -> Self {
        Self { charge_ticks: 0 }
    }

    /// Returns true on the tick the bolt is released. Losing the target or
    /// line of sight drops an incomplete charge, while a latched charge
    /// releases after the full use duration.
    pub fn tick(&mut self, has_target_and_sight: bool) -> bool {
        if !has_target_and_sight {
            self.charge_ticks = 0;
            return false;
        }
        self.charge_ticks += 1;
        if self.charge_ticks < Self::CHARGE_TICKS {
            return false;
        }
        self.charge_ticks = 0;
        true
    }
}

/// Slime/magma-cube mitosis. One draw in `0..3` makes 2–4 children; sizes
/// halve 4→2→1 and size-one cubes do not split.
pub fn ai_slime_split_children(size: u8, roll_0_to_2: u8) -> Option<(u8, u8)> {
    let child = match size {
        4 => 2,
        2 => 1,
        _ => return None,
    };
    Some((child, 2 + roll_0_to_2.min(2)))
}

/// Phantom targeting uses the victim's `time_since_rest` statistic: insomnia
/// starts at 72,000 ticks (three full in-game days).
pub const AI_PHANTOM_INSOMNIA_TICKS: u32 = 72_000;

pub fn ai_phantom_can_target_insomniac(time_since_rest: u32) -> bool {
    time_since_rest >= AI_PHANTOM_INSOMNIA_TICKS
}

/// Wolf variant selection by 1.21.1 spawn biome. Unlisted biomes produce the
/// classic pale wolf.
pub fn ai_wolf_variant_for_biome(biome: &str) -> &'static str {
    match biome.strip_prefix("minecraft:").unwrap_or(biome) {
        "forest" => "minecraft:woods",
        "snowy_taiga" => "minecraft:ashen",
        "old_growth_pine_taiga" => "minecraft:black",
        "old_growth_spruce_taiga" => "minecraft:chestnut",
        "jungle" | "sparse_jungle" | "bamboo_jungle" => "minecraft:rusty",
        "savanna" | "savanna_plateau" | "windswept_savanna" => "minecraft:spotted",
        "badlands" | "eroded_badlands" | "wooded_badlands" => "minecraft:striped",
        "grove" => "minecraft:snowy",
        _ => "minecraft:pale",
    }
}

/// Vanilla wolf pack alerts scan a 32-block radius. A tame wolf joins only
/// when its owner was attacked; wild pack members join a wild packmate.
pub fn ai_wolf_pack_member_joins_anger(
    horizontal_distance_sq: f64,
    member_tamed: bool,
    attacker_is_member_owner: bool,
) -> bool {
    horizontal_distance_sq <= 32.0 * 32.0 && (!member_tamed || attacker_is_member_owner)
}

/// Wolf armor absorbs damage types outside vanilla's `bypasses_wolf_armor`
/// tag; the armor item, not the wolf health pool, pays that absorbed damage.
pub fn ai_wolf_armor_absorbs(damage_bypasses_wolf_armor: bool, armor_present: bool) -> bool {
    armor_present && !damage_bypasses_wolf_armor
}

/// Armadillo threat roll-up. Recent damage (including a close attacker),
/// undead mobs, and sprinting or mounted players inside the seven-block
/// horizontal threat radius roll it up. Vanilla suppresses roll-up while the
/// armadillo is fleeing, airborne, leashed, in water, or on fire; the caller
/// folds those states into `movement_suppressed`.
pub fn ai_armadillo_should_roll_up(
    recently_hurt_or_attacker_near: bool,
    undead_horizontal_distance_sq: Option<f64>,
    player_horizontal_distance_sq: Option<f64>,
    player_sprinting_or_mounted: bool,
    movement_suppressed: bool,
) -> bool {
    if movement_suppressed {
        return false;
    }
    let undead_threat = undead_horizontal_distance_sq.is_some_and(|dist| dist <= 7.0 * 7.0);
    let player_threat = player_sprinting_or_mounted
        && player_horizontal_distance_sq.is_some_and(|dist| dist <= 7.0 * 7.0);
    recently_hurt_or_attacker_near || undead_threat || player_threat
}

/// Brushing an adult armadillo yields exactly one armadillo scute.
pub fn ai_armadillo_brush_drop(adult: bool) -> Option<&'static str> {
    adult.then_some("minecraft:armadillo_scute")
}

/// Frog tongue prey and froglight conversion. Only size-one slime or magma
/// cubes fit; magma cubes convert by frog variant.
pub fn ai_frog_tongue_prey(entity: &str, size: u8) -> bool {
    size == 1 && matches!(entity.strip_prefix("minecraft:").unwrap_or(entity), "slime" | "magma_cube")
}

pub fn ai_froglight_for_variant(variant: &str) -> &'static str {
    match variant.strip_prefix("minecraft:").unwrap_or(variant) {
        "warm" => "minecraft:pearlescent_froglight",
        "cold" => "minecraft:verdant_froglight",
        _ => "minecraft:ochre_froglight",
    }
}

/// A hive/nest gains one honey level when a nectar-carrying bee completes the
/// 2400-tick interior stay; honey is capped at level 5.
pub fn ai_bee_honey_level_on_exit(
    current_level: u8,
    carried_nectar: bool,
    ticks_inside: u32,
    bonus_roll_0_to_99: u8,
) -> u8 {
    if carried_nectar && ticks_inside >= 2_400 {
        // Vanilla normally increments by one and has a 1% chance to increment
        // by a second level on the same exit.
        (current_level + 1 + u8::from(bonus_roll_0_to_99 == 0)).min(5)
    } else {
        current_level.min(5)
    }
}

/// Bee colony persistent anger lasts 400 ticks. Campfire smoke suppresses the
/// harvest provocation but never a direct attack.
pub fn ai_bee_colony_anger_ticks(
    direct_attack: bool,
    harvested: bool,
    campfire_below: bool,
    roll_0_to_380: u16,
) -> u32 {
    if direct_attack || (harvested && !campfire_below) {
        // `PERSISTENT_ANGER_TIME` is uniform in 20..39 seconds, inclusive.
        400 + u32::from(roll_0_to_380.min(380))
    } else {
        0
    }
}

/// Allay note-block affinity: the last heard note block remains its delivery
/// target for 600 ticks.
pub fn ai_allay_note_block_target(heard_at_tick: u64, now_tick: u64) -> bool {
    now_tick.saturating_sub(heard_at_tick) < 600
}

/// Allays collect only matching, untargeted ground items within 32 blocks.
pub fn ai_allay_can_collect_item(
    same_item_and_components: bool,
    targeted_by_player_uuid: bool,
    pickup_delay_elapsed: bool,
    distance_sq: f64,
) -> bool {
    same_item_and_components && !targeted_by_player_uuid && pickup_delay_elapsed && distance_sq <= 32.0 * 32.0
}

/// Sniffer ancient-seed digging: the two-entry vanilla loot table is uniform.
pub fn ai_sniffer_dug_seed(roll_0_or_1: u8) -> &'static str {
    if roll_0_or_1 == 0 { "minecraft:torchflower_seeds" } else { "minecraft:pitcher_pod" }
}

/// Camel dash state: two riders may mount, but sitting camels cannot dash and
/// a dash cannot restart inside the 55-tick cooldown.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AiCamelDash {
    pub last_dash_tick: Option<u64>,
}

impl AiCamelDash {
    pub const COOLDOWN_TICKS: u64 = 55;
    pub const MAX_PASSENGERS: usize = 2;

    /// Returns the horizontal dash impulse; entity physics owns gravity and
    /// the upward component.
    pub fn try_dash(&mut self, now_tick: u64, sitting: bool, yaw_degrees: f32) -> Option<(f64, f64)> {
        if sitting
            || self.last_dash_tick.is_some_and(|last| now_tick.saturating_sub(last) < Self::COOLDOWN_TICKS)
        {
            return None;
        }
        self.last_dash_tick = Some(now_tick);
        let yaw = yaw_degrees.to_radians();
        Some((-yaw.sin() as f64 * 1.15, yaw.cos() as f64 * 1.15))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum MobGoalKind {
    Float,
    Swell,
    MeleeAttack,
    RangedSnowball,
    /// `RangedBowAttackGoal`; the payload names the tipped-arrow effect for
    /// strays and bogged, `None` for a plain skeleton arrow.
    RangedBow {
        tipped: Option<&'static str>,
    },
    AvoidEntity(AvoidEntitySpec),
    WaterAvoidingRandomStroll,
    LookAtPlayer,
    RandomLookAround,
    LeapAtTarget,
    SpiderAttack,
    ZombieAttack,
    BreakDoor,
    ZombieAttackTurtleEgg,
    RestrictSun,
    FleeSun,
    MoveThroughVillage {
        can_break_doors: bool,
    },
    OpenDoor,
}

/// Vanilla `AvoidEntityGoal` constants (maxDist 6.0, walk 1.0, sprint 1.2).
const fn avoid(flee_etypes: &'static [i64], require_unscared: bool) -> AvoidEntitySpec {
    AvoidEntitySpec {
        flee_etypes,
        require_unscared,
        max_dist: 6.0,
        walk_speed: 1.0,
        sprint_speed: 1.2,
    }
}

const AVOID_OCELOT: AvoidEntitySpec = avoid(&[72], false);
const AVOID_CAT: AvoidEntitySpec = avoid(&[15], false);
const AVOID_WOLF: AvoidEntitySpec = avoid(&[122], false);
const AVOID_UNROLLED_ARMADILLO: AvoidEntitySpec = avoid(&[2], true);

/// The equipment and door-breaking state that changes vanilla goal registration.
#[derive(Clone, Copy, Default)]
pub(crate) struct MobGoalSettings {
    pub skeleton_has_bow: bool,
    pub zombie_can_break_doors: bool,
}

pub(crate) struct MobGoalPlan {
    /// Ordered by priority, preserving vanilla's registration order for ties.
    pub goals: Vec<(i32, MobGoalKind)>,
    /// Target goals need inputs the current engine does not supply.
    #[cfg(test)]
    pub missing_targets: &'static [(i32, &'static str)],
    /// The mob's eye height; look and bow goals place their points on it.
    eye_height: f64,
}

/// The Zombie goal table's 1.21.1 subclass owners: husk, drowned,
/// zombified piglin and zombie villager all inherit `Zombie.registerGoals`.
const ZOMBIE_FAMILY: &[&str] = &[
    "minecraft:zombie",
    "minecraft:husk",
    "minecraft:drowned",
    "minecraft:zombie_villager",
    "minecraft:zombified_piglin",
];

// `registerGoals` tables (1.21.1) in vanilla registration order. The stable
// priority sort in `GoalSelector::add_goal` keeps that order inside a priority,
// which is how vanilla resolves ties between equal-priority goals.
const ZOMBIE_GOALS: &[(i32, MobGoalKind)] = &[
    (2, MobGoalKind::ZombieAttack),
    (4, MobGoalKind::ZombieAttackTurtleEgg),
    (6, MobGoalKind::MoveThroughVillage { can_break_doors: false }),
    (7, MobGoalKind::WaterAvoidingRandomStroll),
    (8, MobGoalKind::LookAtPlayer),
    (8, MobGoalKind::RandomLookAround),
];

// Stray and Bogged extend AbstractSkeleton and register Skeleton's table, each
// with its own tipped arrow; `SKELETON_ATTACK_GOAL` swaps in the bow variant.
const SKELETON_GOALS: &[(i32, MobGoalKind)] = &[
    (2, MobGoalKind::RestrictSun),
    (3, MobGoalKind::FleeSun),
    (3, MobGoalKind::AvoidEntity(AVOID_WOLF)),
    (4, MobGoalKind::MeleeAttack),
    (5, MobGoalKind::WaterAvoidingRandomStroll),
    (6, MobGoalKind::LookAtPlayer),
    (6, MobGoalKind::RandomLookAround),
];

const CREEPER_GOALS: &[(i32, MobGoalKind)] = &[
    (1, MobGoalKind::Float),
    (2, MobGoalKind::Swell),
    (3, MobGoalKind::AvoidEntity(AVOID_OCELOT)),
    (3, MobGoalKind::AvoidEntity(AVOID_CAT)),
    (4, MobGoalKind::MeleeAttack),
    (5, MobGoalKind::WaterAvoidingRandomStroll),
    (6, MobGoalKind::LookAtPlayer),
    (6, MobGoalKind::RandomLookAround),
];

// CaveSpider inherits Spider.registerGoals unchanged in 1.21.1.
const SPIDER_GOALS: &[(i32, MobGoalKind)] = &[
    (1, MobGoalKind::Float),
    (2, MobGoalKind::AvoidEntity(AVOID_UNROLLED_ARMADILLO)),
    (3, MobGoalKind::LeapAtTarget),
    (4, MobGoalKind::SpiderAttack),
    (5, MobGoalKind::WaterAvoidingRandomStroll),
    (6, MobGoalKind::LookAtPlayer),
    (6, MobGoalKind::RandomLookAround),
];

const SNOW_GOLEM_GOALS: &[(i32, MobGoalKind)] = &[
    (1, MobGoalKind::RangedSnowball),
    (2, MobGoalKind::WaterAvoidingRandomStroll),
    (3, MobGoalKind::LookAtPlayer),
    (4, MobGoalKind::RandomLookAround),
];

const VILLAGER_GOALS: &[(i32, MobGoalKind)] = &[
    (1, MobGoalKind::OpenDoor),
    (5, MobGoalKind::WaterAvoidingRandomStroll),
    (6, MobGoalKind::LookAtPlayer),
    (7, MobGoalKind::RandomLookAround),
];

const VINDICATOR_GOALS: &[(i32, MobGoalKind)] = &[
    (1, MobGoalKind::OpenDoor),
    (2, MobGoalKind::MeleeAttack),
    (5, MobGoalKind::WaterAvoidingRandomStroll),
    (6, MobGoalKind::LookAtPlayer),
    (7, MobGoalKind::RandomLookAround),
];

// Target goals the current engine cannot build: the vanilla
// `registerTargetGoals` inventory for the pinned protocol-767 registry.
const ZOMBIE_MISSING_TARGETS: &[(i32, &str)] = &[
    (1, "HurtByTargetGoal (alerts allies, including zombified piglins)"),
    (2, "NearestAttackableTargetGoal<Player>"),
    (3, "NearestAttackableTargetGoal<AbstractVillager>"),
    (3, "NearestAttackableTargetGoal<IronGolem>"),
    (5, "NearestAttackableTargetGoal<Turtle> (baby on land)"),
];

const SKELETON_MISSING_TARGETS: &[(i32, &str)] = &[
    (1, "HurtByTargetGoal"),
    (2, "NearestAttackableTargetGoal<Player>"),
    (3, "NearestAttackableTargetGoal<IronGolem>"),
    (3, "NearestAttackableTargetGoal<Turtle> (baby on land)"),
];

const CREEPER_MISSING_TARGETS: &[(i32, &str)] = &[
    (1, "NearestAttackableTargetGoal<Player>"),
    (2, "HurtByTargetGoal"),
];

const SPIDER_MISSING_TARGETS: &[(i32, &str)] = &[
    (1, "HurtByTargetGoal"),
    (2, "SpiderTargetGoal<Player>"),
    (3, "SpiderTargetGoal<IronGolem>"),
];

/// The mobs whose every vanilla target goal is already implemented.
const NO_MISSING_TARGETS: &[(i32, &str)] = &[];

/// Table slots whose goal depends on registration-time equipment state.
const ZOMBIE_VILLAGE_GOAL: usize = 2;
const SKELETON_ATTACK_GOAL: usize = 3;

/// Stray and Bogged extend AbstractSkeleton in 1.21.1 and register its goals.
fn is_skeleton_etype(name: &str) -> bool {
    matches!(
        name,
        "minecraft:skeleton" | "minecraft:stray" | "minecraft:bogged"
    )
}

/// Tipped-arrow effect a bow-armed skeleton family member looses.
fn skeleton_tipped_arrow(name: &str) -> Option<&'static str> {
    match name {
        "minecraft:stray" => Some("minecraft:slowness"),
        "minecraft:bogged" => Some("minecraft:poison"),
        _ => None,
    }
}

impl MobGoalPlan {
    pub fn for_entity(etype: i64, settings: MobGoalSettings) -> Option<Self> {
        use MobGoalKind::*;
        let entity = entity_spec_by_id(etype)?;
        let zombie = ZOMBIE_FAMILY.contains(&entity.name);
        let (base, missing_targets) = match entity.name {
            _ if zombie => (ZOMBIE_GOALS, ZOMBIE_MISSING_TARGETS),
            name if is_skeleton_etype(name) => (SKELETON_GOALS, SKELETON_MISSING_TARGETS),
            "minecraft:creeper" => (CREEPER_GOALS, CREEPER_MISSING_TARGETS),
            "minecraft:spider" | "minecraft:cave_spider" => (SPIDER_GOALS, SPIDER_MISSING_TARGETS),
            // The Enemy-mob target goal is implemented: it lives in
            // `build_target_selector_for_etype`.
            "minecraft:snow_golem" => (SNOW_GOLEM_GOALS, NO_MISSING_TARGETS),
            "minecraft:villager" => (VILLAGER_GOALS, NO_MISSING_TARGETS),
            "minecraft:vindicator" => (VINDICATOR_GOALS, NO_MISSING_TARGETS),
            _ => return None,
        };
        let mut goals = base.to_vec();
        // Equipment and griefing gates vanilla evaluates while registering.
        if zombie {
            goals[ZOMBIE_VILLAGE_GOAL].1 = MoveThroughVillage {
                can_break_doors: settings.zombie_can_break_doors,
            };
            if settings.zombie_can_break_doors {
                goals.insert(0, (1, BreakDoor));
            }
        } else if settings.skeleton_has_bow && is_skeleton_etype(entity.name) {
            goals[SKELETON_ATTACK_GOAL].1 = RangedBow {
                tipped: skeleton_tipped_arrow(entity.name),
            };
        }
        let _ = missing_targets;
        Some(Self {
            goals,
            #[cfg(test)]
            missing_targets,
            eye_height: f64::from(entity.eye_height),
        })
    }

    pub fn build_selector(&self, seed: u64) -> GoalSelector {
        let mut selector = GoalSelector::default();
        for &(priority, kind) in &self.goals {
            let goal: Box<dyn Goal> = match kind {
                MobGoalKind::Float => Box::new(FloatGoal { priority }),
                // Only creepers swell; their fuse yields to the cat/ocelot flee.
                MobGoalKind::Swell => {
                    Box::new(SwellGoal::new(priority).aborted_by(&[72, 15], 6.0))
                }
                MobGoalKind::MeleeAttack => Box::new(MeleeAttackGoal::new(priority)),
                MobGoalKind::RangedSnowball => Box::new(RangedSnowballGoal::new(priority)),
                MobGoalKind::RangedBow { tipped } => Box::new(RangedBowAttackGoal::new(
                    priority,
                    self.eye_height,
                    tipped,
                    seed,
                )),
                MobGoalKind::AvoidEntity(spec) => Box::new(AvoidEntityGoal::new(priority, spec)),
                MobGoalKind::WaterAvoidingRandomStroll => {
                    Box::new(WaterAvoidingRandomStrollGoal::new(priority, seed))
                }
                MobGoalKind::LookAtPlayer => Box::new(LookAtPlayerGoal {
                    priority,
                    look_distance: 8.0,
                }),
                MobGoalKind::RandomLookAround => {
                    Box::new(RandomLookAroundGoal::new(priority, self.eye_height, seed))
                }
                MobGoalKind::LeapAtTarget => Box::new(LeapAtTargetGoal::new(priority, seed)),
                MobGoalKind::SpiderAttack => Box::new(SpiderAttackGoal::new(priority)),
                MobGoalKind::ZombieAttack => Box::new(ZombieAttackGoal::new(priority)),
                MobGoalKind::BreakDoor => Box::new(BreakDoorGoal::new(priority)),
                MobGoalKind::ZombieAttackTurtleEgg => {
                    Box::new(ZombieAttackTurtleEggGoal::new(priority, seed))
                }
                MobGoalKind::RestrictSun => Box::new(RestrictSunGoal::new(priority)),
                MobGoalKind::FleeSun => Box::new(FleeSunGoal::new(priority)),
                MobGoalKind::MoveThroughVillage { can_break_doors } => Box::new(
                    MoveThroughVillageGoal::new(priority, can_break_doors, seed),
                ),
                MobGoalKind::OpenDoor => Box::new(OpenDoorGoal::new(priority)),
            };
            selector.add_goal(goal);
        }
        selector
    }
}

/// Selects the goal schedule from the pinned protocol-767 entity registry.
/// Goals are stateful, so a selector is constructed once for each mob.
pub(crate) fn build_goal_selector_for_etype(etype: i64) -> GoalSelector {
    let Some(spec) = entity_spec_by_id(etype) else {
        return GoalSelector::default();
    };
    if spec.max_health <= 0.0 || matches!(spec.name, "minecraft:player" | "minecraft:armor_stand") {
        return GoalSelector::default();
    }

    let seed = (etype as u64) ^ 0x9e3779b97f4a7c15;
    // The live schedule registers every vanilla goal; the bow and door
    // predicates are still checked inside each goal's `canUse` exactly like
    // vanilla's equipment/difficulty gates.
    let settings = MobGoalSettings {
        skeleton_has_bow: true,
        zombie_can_break_doors: true,
    };
    if let Some(plan) = MobGoalPlan::for_entity(etype, settings) {
        let mut selector = plan.build_selector(seed);
        // The tick driver has fluid observations for the float behavior.
        if ZOMBIE_FAMILY.contains(&spec.name) || is_skeleton_etype(spec.name) {
            selector.add_goal(Box::new(FloatGoal { priority: 0 }));
        }
        return selector;
    }

    let mut selector = GoalSelector::default();
    selector.add_goal(Box::new(FloatGoal { priority: 0 }));
    match spec.name {
        // MeleeAttackGoal so the new mob-vs-mob target goals feed an attack.
        "minecraft:iron_golem" => selector.add_goal(Box::new(MeleeAttackGoal::new(1))),
        "minecraft:fox" => selector.add_goal(Box::new(MeleeAttackGoal::new(4))),
        "minecraft:wolf" => selector.add_goal(Box::new(MeleeAttackGoal::new(7))),
        "minecraft:cow" | "minecraft:pig" | "minecraft:sheep" | "minecraft:chicken" => {
            // Item IDs are the existing 1.21.1 server item mappings.
            let tempting_item = match spec.name {
                "minecraft:pig" => 1097,    // carrot
                "minecraft:chicken" => 853, // wheat_seeds
                _ => 854,                   // wheat
            };
            selector.add_goal(Box::new(PanicGoal::new(1)));
            selector.add_goal(Box::new(BreedGoal::new(2)));
            selector.add_goal(Box::new(TemptGoal::new(3, tempting_item)));
        }
        _ => {}
    }
    selector.add_goal(Box::new(WaterAvoidingRandomStrollGoal::new(6, seed)));
    selector.add_goal(Box::new(LookAtPlayerGoal {
        priority: 7,
        look_distance: 8.0,
    }));
    selector
}

/// Per-entity persistent AI state. Keep one profile across ticks so attack
/// cooldowns, breeding progress and navigation waypoints are retained.
pub(crate) struct MobAiProfile {
    pub etype: i64,
    pub selector: GoalSelector,
    /// Vanilla `targetSelector` for mob-vs-mob targets (empty for most mobs).
    pub target_selector: TargetSelector,
    /// Mob-vs-mob observation fed to both selectors each step. Stays empty
    /// until a caller supplies nearby mobs, which keeps callers that only
    /// track players unchanged.
    pub perception: MobPerception,
    pub navigation: PathNavigation,
    pub movement_speed: f64,
    pub eye_height: f64,
    requested_target: Option<(i32, i32, i32)>,
}

impl MobAiProfile {
    pub fn new(etype: i64) -> Self {
        let spec = entity_spec_by_id(etype);
        let movement_speed = if matches!(etype, 19 | 22 | 77 | 87) {
            0.2
        } else {
            0.25
        };
        Self {
            etype,
            selector: build_goal_selector_for_etype(etype),
            target_selector: build_target_selector_for_etype(etype),
            perception: MobPerception::default(),
            navigation: PathNavigation::default(),
            movement_speed,
            eye_height: spec.map_or(1.53, |spec| f64::from(spec.eye_height)),
            requested_target: None,
        }
    }

    /// Vanilla ticks `targetSelector` first: a mob-vs-mob target chosen this
    /// tick already feeds the goal selector, and a supplied player target wins.
    fn mob_target_perception(&self, targets: &GoalOutput) -> Option<AiEntityPerception> {
        let mob = self
            .perception
            .nearby_mobs
            .iter()
            .find(|mob| Some(mob.entity_id) == targets.mob_target)?;
        let mut target = AiEntityPerception::new(mob.entity_id, mob.position);
        target.etype = mob.etype;
        target.line_of_sight = mob.line_of_sight;
        target.width = entity_spec_by_id(mob.etype).map_or(0.6, |spec| spec.width);
        Some(target)
    }

    /// Re-paths toward the goal's move target when it changed or the route ran
    /// out, then returns the waypoint the mob strides toward this tick.
    fn follow_path(&mut self, ctx: &GoalContext<'_>, goal: &GoalOutput) -> Option<(f64, f64, f64)> {
        let (Some(world), Some(target)) = (ctx.world, goal.move_target) else {
            // No world to route in (or nothing to route to): drop the stale
            // route and let the goal's own target stand as the waypoint.
            self.navigation.stop();
            self.requested_target = None;
            return goal.move_target;
        };
        let destination = floor_cell(target);
        if self.requested_target != Some(destination) || self.navigation.is_done() {
            let finder = PathFinder {
                evaluator: WalkNodeEvaluator {
                    mob_height: ctx.height,
                    can_pass_closed_wooden_doors: door_capable(self.etype),
                    ..WalkNodeEvaluator::default()
                },
                ..PathFinder::default()
            };
            self.navigation
                .navigate_to(&finder, world, floor_cell(ctx.position), destination);
            self.requested_target = Some(destination);
        }
        self.navigation
            .advance_if_reached(world, ctx.position, 0.45);
        self.navigation.next_waypoint(world)
    }
}

/// Intent to be applied by the live mob physics and interaction code.
/// Movement is an X/Z displacement per tick; Y movement stays with physics.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct MobAiStepResult {
    pub move_target: Option<(f64, f64, f64)>,
    pub movement_delta: (f64, f64, f64),
    pub look_yaw: Option<f32>,
    pub look_pitch: Option<f32>,
    pub jump: bool,
    pub attack_target: Option<i32>,
    /// Mob-vs-mob target chosen by the target selector this tick.
    pub mob_target: Option<i32>,
    /// Ranged-goal projectile launch (e.g. a snow golem's snowball).
    pub ranged_attack: Option<RangedProjectileAction>,
    pub swell: Option<bool>,
    pub breed_target: Option<i32>,
    /// Block cell a goal wants removed this tick (turtle-egg stomp, door
    /// break finish).
    pub destroy_block: Option<(i32, i32, i32)>,
    /// `BreakDoorGoal`'s `destroyBlockProgress` broadcast: door cell plus
    /// the vanilla crack stage.
    pub door_break_progress: Option<((i32, i32, i32), u8)>,
    /// Zombie `setAggressive` arm-raise state while its attack goal runs.
    pub arms_raised: Option<bool>,
    /// `RestrictSunGoal`'s navigation sun-avoidance restriction.
    pub restrict_sun: Option<bool>,
    /// Closed wooden door cell the AI wants opened this tick.
    pub open_door: Option<(i32, i32, i32)>,
    /// `maxEntityCramming` damage evaluated for this tick; the live push
    /// engine applies it through its own hurt path.
    pub cramming_damage: f32,
}

/// One tick of scheduling, path-following and intent generation. The caller
/// supplies observations and a PathWorld, then applies the returned actions.
pub(crate) fn tick_mob_ai_step(
    profile: &mut MobAiProfile,
    ctx: &GoalContext<'_>,
) -> MobAiStepResult {
    profile.selector.perception.clone_from(&profile.perception);
    profile
        .target_selector
        .perception
        .clone_from(&profile.perception);
    // Vanilla ticks the target selector first, so a mob-vs-mob target chosen
    // this tick already feeds the goal selector.
    let targets = profile.target_selector.tick(ctx);
    let ctx = &GoalContext {
        target: ctx.target.or_else(|| profile.mob_target_perception(&targets)),
        ..*ctx
    };
    let mut goal = profile.selector.tick(ctx);
    goal.merge_missing(targets);
    let cramming_damage = step_cramming_damage(ctx);
    // O(1) idle tick: no goal owns a channel this tick, so the route is dropped
    // and only the cramming evaluation reaches the caller.
    if goal == GoalOutput::default() {
        profile.navigation.stop();
        profile.requested_target = None;
        return MobAiStepResult {
            cramming_damage,
            ..MobAiStepResult::default()
        };
    }
    let waypoint = profile.follow_path(ctx, &goal);
    let (movement_delta, jump) = stride(ctx, profile, &goal, waypoint);
    let (look_yaw, look_pitch) = look_direction(ctx, profile.eye_height, goal.look_target);

    MobAiStepResult {
        move_target: goal.move_target,
        movement_delta,
        look_yaw,
        look_pitch,
        jump,
        attack_target: goal.attack_target,
        mob_target: goal.mob_target,
        ranged_attack: goal.ranged_attack,
        swell: goal.swell,
        breed_target: goal.breed_target,
        destroy_block: goal.destroy_block,
        door_break_progress: goal.door_break_progress,
        arms_raised: goal.arms_raised,
        restrict_sun: goal.restrict_sun,
        open_door: goal.open_door,
        cramming_damage,
    }
}

/// Block cell a world-space point stands in.
fn floor_cell(point: (f64, f64, f64)) -> (i32, i32, i32) {
    (
        point.0.floor() as i32,
        point.1.floor() as i32,
        point.2.floor() as i32,
    )
}

/// Vanilla door-capable navigators: villagers and vindicators open a wooden
/// door, the zombie and drowned families break one.
fn door_capable(etype: i64) -> bool {
    matches!(etype, 27 | 54 | 113 | 114 | 124 | 126 | 127)
}

/// Waypoint stride plus the goal-owned momentum that replaces it — a leap
/// impulse or a bow strafe. The live mover applies `movement_delta` verbatim.
fn stride(
    ctx: &GoalContext<'_>,
    profile: &MobAiProfile,
    goal: &GoalOutput,
    waypoint: Option<(f64, f64, f64)>,
) -> ((f64, f64, f64), bool) {
    let mut movement_delta = (0.0, 0.0, 0.0);
    let mut jump = goal.jump;
    if let Some(point) = waypoint {
        let dx = point.0 - ctx.position.0;
        let dz = point.2 - ctx.position.2;
        let distance = dx.hypot(dz);
        if distance > 0.05 {
            let speed = profile.movement_speed * goal.speed_modifier.unwrap_or(1.0);
            let step = speed.max(0.0).min(distance);
            movement_delta = (dx / distance * step, 0.0, dz / distance * step);
        }
        jump |= point.1 > ctx.position.1 + 0.5;
    }
    if let Some((dx, dz)) = goal.move_impulse {
        movement_delta = (dx, 0.0, dz);
    }
    // minecraft:drowned — DrownedSwimGoal chases through the water column.
    if profile.etype == 27 && ctx.in_water {
        if let Some(point) = goal.move_target {
            let swim = drowned_swim_chase_velocity(
                [ctx.position.0, ctx.position.1, ctx.position.2],
                [point.0, point.1, point.2],
            );
            movement_delta = (swim[0], 0.0, swim[2]);
        }
    }
    (movement_delta, jump)
}

/// Yaw/pitch toward a goal's look point, measured from the mob's eye height.
fn look_direction(
    ctx: &GoalContext<'_>,
    eye_height: f64,
    look_target: Option<(f64, f64, f64)>,
) -> (Option<f32>, Option<f32>) {
    let Some(point) = look_target else {
        return (None, None);
    };
    let dx = point.0 - ctx.position.0;
    let dz = point.2 - ctx.position.2;
    let dy = point.1 - (ctx.position.1 + eye_height);
    let horizontal_distance = dx.hypot(dz);
    if horizontal_distance == 0.0 && dy == 0.0 {
        return (None, None);
    }
    (
        Some((-dx.atan2(dz)).to_degrees() as f32),
        Some((-dy.atan2(horizontal_distance)).to_degrees() as f32),
    )
}

/// `maxEntityCramming`: the live push engine owns the damage application; the
/// step only reports this tick's evaluation for the caller.
fn step_cramming_damage(ctx: &GoalContext<'_>) -> f32 {
    let Some(world) = ctx.world else {
        return 0.0;
    };
    let Some((pushable, non_passengers, roll_mod_4)) =
        world.cramming_census(ctx.position.0, ctx.position.1, ctx.position.2)
    else {
        return 0.0;
    };
    eval_entity_cramming_damage(
        max_entity_cramming_limit(),
        pushable,
        non_passengers,
        roll_mod_4,
    )
}

#[cfg(test)]
mod mob_ai_tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct GridWorld {
        blocks: BTreeMap<(i32, i32, i32), BlockPathType>,
        heights: BTreeMap<(i32, i32, i32), f64>,
        names: BTreeMap<(i32, i32, i32), &'static str>,
    }

    impl GridWorld {
        fn set(&mut self, x: i32, y: i32, z: i32, ty: BlockPathType) {
            self.blocks.insert((x, y, z), ty);
        }

        fn name(&mut self, x: i32, y: i32, z: i32, name: &'static str) {
            self.names.insert((x, y, z), name);
        }

        fn wall(&mut self, x: i32, z: i32, height: i32) {
            for y in 1..=height {
                self.set(x, y, z, BlockPathType::Blocked);
            }
        }
    }

    impl PathWorld for GridWorld {
        fn path_type(&self, x: i32, y: i32, z: i32) -> BlockPathType {
            self.blocks.get(&(x, y, z)).copied().unwrap_or_else(|| {
                if y <= 0 {
                    BlockPathType::Blocked
                } else if self.is_solid(x, y - 1, z) {
                    BlockPathType::Walkable
                } else {
                    BlockPathType::Open
                }
            })
        }
        fn is_solid(&self, x: i32, y: i32, z: i32) -> bool {
            self.blocks.get(&(x, y, z)).map_or(y <= 0, |ty| {
                matches!(
                    ty,
                    BlockPathType::Blocked | BlockPathType::Fence | BlockPathType::Leaves
                )
            })
        }
        fn collision_height(&self, x: i32, y: i32, z: i32) -> f64 {
            self.heights.get(&(x, y, z)).copied().unwrap_or_else(|| {
                if self.is_solid(x, y, z) {
                    1.0
                } else {
                    0.0
                }
            })
        }
        fn min_y(&self) -> i32 {
            -64
        }
        fn max_y(&self) -> i32 {
            320
        }
        fn block_name(&self, x: i32, y: i32, z: i32) -> &str {
            self.names.get(&(x, y, z)).copied().unwrap_or("minecraft:air")
        }
    }

    fn positions(path: &AiPath) -> Vec<(i32, i32, i32)> {
        path.nodes
            .iter()
            .map(|node| (node.x, node.y, node.z))
            .collect()
    }

    fn target(id: i32, x: f64, y: f64, z: f64) -> AiEntityPerception {
        AiEntityPerception::new(id, (x, y, z))
    }

    fn mob_at(etype: i64, eid: i32, x: f64, z: f64) -> NearbyMob {
        NearbyMob::new(eid, etype, (x, 1.0, z))
    }

    #[cfg(megu_vanilla_data)]
    fn state(name: &str, props: &[(&str, &str)]) -> u32 {
        crate::block_registry::breg()
            .unwrap()
            .checked_state(
                name,
                &props
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .collect::<Vec<_>>(),
            )
            .unwrap_or_else(|| panic!("missing block state {name} {props:?}"))
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn world_path_raw_types_match_pinned_vanilla_digest() {
        let registry = crate::block_registry::breg().unwrap();
        // FNV-1a of one PascalCase PathType name plus newline per raw state ID.
        // Captured from 1.21.1 WalkNodeEvaluator.getPathTypeFromState with vanilla tags bound.
        assert_eq!(registry.state_count(), 26_684);
        let mut digest = megu_core::hash::Fnv1a64::new();
        for state in 0..registry.state_count() as u32 {
            let path_type = classify_block_state_path_type(registry, state);
            digest.update_str(&format!("{path_type:?}\n"));
        }
        assert_eq!(digest.finish(), 0x54a74560ce1a30cc);
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn real_registry_block_path_types_and_properties() {
        use BlockPathType::*;
        let registry = crate::block_registry::breg().unwrap();
        for (name, props, expected) in [
            ("air", &[][..], Open),
            ("cave_air", &[], Open),
            ("short_grass", &[], Open),
            ("bubble_column", &[], Water),
            ("oak_fence", &[], Fence),
            ("cobblestone_wall", &[], Fence),
            ("rail", &[], Rail),
            ("oak_leaves", &[], Leaves),
            ("oak_trapdoor", &[], Trapdoor),
            ("stone", &[], Blocked),
            ("nether_brick_fence", &[], Fence),
            ("tuff_wall", &[], Fence),
            ("oak_fence_gate", &[("open", "false")], Fence),
            ("oak_fence_gate", &[("open", "true")], Open),
            ("lava", &[("level", "7")], Lava),
            ("water", &[("level", "7")], Water),
            ("ladder", &[("waterlogged", "true")], Water),
            ("oak_slab", &[("waterlogged", "true")], Blocked),
            ("oak_stairs", &[("waterlogged", "true")], Blocked),
            ("fire", &[], DamageFire),
            ("soul_fire", &[], DamageFire),
            ("lava_cauldron", &[], DamageFire),
            ("magma_block", &[], DamageFire),
            ("campfire", &[("lit", "true")], DamageFire),
            ("campfire", &[("lit", "false")], Blocked),
            ("soul_campfire", &[("lit", "true")], DamageFire),
            ("soul_campfire", &[("lit", "false")], Blocked),
            ("cactus", &[], DamageOther),
            ("sweet_berry_bush", &[("age", "0")], DamageOther),
            ("wither_rose", &[], DamageCautious),
            ("pointed_dripstone", &[], DamageCautious),
            ("iron_trapdoor", &[("open", "true")], Trapdoor),
            ("lily_pad", &[], Trapdoor),
            ("big_dripleaf", &[("tilt", "full")], Trapdoor),
            ("oak_door", &[("open", "false")], DoorWoodClosed),
            ("oak_door", &[("open", "true")], DoorOpen),
            ("waxed_copper_door", &[("open", "false")], DoorWoodClosed),
            ("iron_door", &[("open", "true")], DoorOpen),
            ("iron_door", &[("open", "false")], DoorIronClosed),
            ("powered_rail", &[], Rail),
            ("flowering_azalea_leaves", &[], Leaves),
            ("powder_snow", &[], PowderSnow),
            ("cocoa", &[], Cocoa),
            ("honey_block", &[], StickyHoney),
            ("moving_piston", &[], Blocked),
            ("snow", &[("layers", "4")], Open),
            ("snow", &[("layers", "5")], Blocked),
            ("white_carpet", &[], Open),
            ("large_amethyst_bud", &[], Open),
            ("pitcher_crop", &[("age", "3"), ("half", "lower")], Open),
        ] {
            let block = state(name, props);
            assert_eq!(
                classify_block_state_path_type(registry, block),
                expected,
                "{name} {props:?}"
            );
            let adapter = LiveWorldPathAdapter::with_bounds(registry, 0, 4, |_, y, _| match y {
                1 => block,
                _ => 0,
            });
            assert_eq!(adapter.path_type(0, 1, 0), expected, "{name} {props:?}");
        }
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn world_path_classifies_support_and_neighbor_hazards() {
        use BlockPathType::*;
        let registry = crate::block_registry::breg().unwrap();
        let stone = state("stone", &[]);
        for (name, offset, expected) in [
            ("air", (0, 0, 0), Open),
            ("stone", (0, 0, 0), Walkable),
            ("water", (0, 0, 0), Open),
            ("lava", (0, 0, 0), Open),
            ("redstone_torch", (0, 1, 0), Walkable),
            ("fire", (0, 0, 0), DamageFire),
            ("cactus", (0, 0, 0), DamageOther),
            ("wither_rose", (0, 0, 0), DamageCautious),
            ("honey_block", (0, 0, 0), StickyHoney),
            ("powder_snow", (0, 0, 0), DangerPowdersnow),
            ("lily_pad", (0, 0, 0), DangerTrapdoor),
            ("lava", (-1, 1, 0), DangerFire),
            ("fire", (1, 2, 1), DangerFire),
            ("sweet_berry_bush", (1, 0, -1), DangerOther),
            ("wither_rose", (0, 2, 1), DamageCautious),
            ("water", (1, 1, 1), WaterBorder),
            ("powder_snow", (-1, 1, 0), Walkable),
            ("cactus", (0, 2, 0), Walkable),
        ] {
            let block = state(name, &[]);
            let adapter = LiveWorldPathAdapter::with_bounds(registry, 0, 4, |x, y, z| {
                if (x, y, z) == offset {
                    block
                } else if (x, y, z) == (0, 0, 0) {
                    stone
                } else {
                    0
                }
            });
            assert_eq!(adapter.path_type(0, 1, 0), expected, "{name} at {offset:?}");
        }
        let water = state("water", &[]);
        let cactus = state("cactus", &[]);
        let adapter =
            LiveWorldPathAdapter::with_bounds(registry, 0, 4, |x, y, z| match (x, y, z) {
                (0, 0, 0) => stone,
                (-1, 2, 1) => water,
                (1, 1, 0) => cactus,
                _ => 0,
            });
        assert_eq!(adapter.path_type(0, 1, 0), WaterBorder);
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn world_path_uses_collision_tops_for_floor_and_waypoints() {
        let stone = state("stone", &[]);
        let slab = state("oak_slab", &[("type", "bottom")]);
        let fence = state("oak_fence", &[]);
        let water = state("water", &[]);
        let adapter = LiveWorldPathAdapter::with_bounds(
            crate::block_registry::breg().unwrap(),
            -8,
            16,
            |x, y, _| {
                assert!((-8..=16).contains(&y));
                match (x, y) {
                    (0, -2) => stone,
                    (1, -2) => slab,
                    (2, -2) => fence,
                    (3, -2) => water,
                    (5, 16) => stone,
                    _ => 0,
                }
            },
        );
        assert_eq!(adapter.waypoint_y(0, -1, 0), -1.0);
        assert_eq!(adapter.waypoint_y(1, -1, 0), -1.5);
        assert_eq!(adapter.waypoint_y(2, -1, 0), -0.5);
        assert_eq!(adapter.waypoint_y(3, -1, 0), -2.0);
        assert_eq!(adapter.waypoint_y(4, -1, 0), -1.0);
        assert_eq!(adapter.path_type(0, -9, 0), BlockPathType::Open);
        assert_eq!(adapter.path_type(0, 17, 0), BlockPathType::Open);
        assert_eq!(adapter.path_type(5, 17, 0), BlockPathType::Walkable);
        assert!(!WalkNodeEvaluator::default().standable(&adapter, (5, 17, 0)));
        let mut navigation = PathNavigation::default();
        assert!(navigation.navigate_to(&PathFinder::default(), &adapter, (0, -1, 0), (1, -1, 0)));
        assert_eq!(navigation.next_waypoint(&adapter), Some((1.5, -1.5, 0.5)));
        navigation.advance_if_reached(&adapter, (1.5, -1.5, 0.5), 0.1);
        assert!(navigation.is_done());
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn world_path_routes_zombie_around_lava_and_through_open_door() {
        let reg = crate::block_registry::breg().unwrap();
        let stone = state("stone", &[]);
        let lava = state("lava", &[]);
        let door = |half, open| {
            state(
                "iron_door",
                &[("half", half), ("open", open), ("powered", open)],
            )
        };
        let corridor = |lower, upper| {
            LiveWorldPathAdapter::with_bounds(reg, 0, 3, move |x, y, z| match (x, y, z) {
                (2, 1, 1) => lava,
                (4, 1, 1) => lower,
                (4, 2, 1) => upper,
                (4, _, _) => stone,
                (0..=6, 1..=2, 0..=2) => 0,
                _ => stone,
            })
        };
        let opened = corridor(door("lower", "true"), door("upper", "true"));
        let closed = corridor(door("lower", "false"), door("upper", "false"));
        let zombie = entity_spec_by_name("zombie").unwrap();
        let finder = PathFinder {
            evaluator: WalkNodeEvaluator {
                mob_height: f64::from(zombie.height),
                ..WalkNodeEvaluator::default()
            },
            ..PathFinder::default()
        };
        let from = (0, 1, 1);
        let target = (6, 1, 1);
        assert_eq!(opened.collision_height(4, 1, 1), 1.0);
        let path = finder.find_path(&opened, from, target);
        assert!(path.reached);
        assert!(path.nodes.iter().any(|node| {
            (node.x, node.y, node.z) == (4, 1, 1) && node.path_type == BlockPathType::DoorOpen
        }));
        assert!(path.nodes.iter().any(|node| node.x == 2 && node.z != 1));
        assert!(path
            .nodes
            .iter()
            .all(|node| (node.x, node.y, node.z) != (2, 1, 1)));
        assert!(!finder.find_path(&closed, from, target).reached);
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn live_adapter_dimension_bounds_and_real_collision_shapes() {
        let reg = crate::block_registry::breg().expect("1.21.1 block registry");
        let stone = reg.state_of("stone", None);
        let air = reg.state_of("air", None);
        let fence = reg.state_of("oak_fence", None);
        let lava = reg.state_of("lava", None);
        let stairs = reg.state_of("oak_stairs", None);
        let blocks = BTreeMap::from([
            ((1, 1, 0), fence),
            ((2, 1, 0), lava),
            ((3, 1, 0), stairs),
            ((1, 2, 0), stone),
            ((2, 2, 0), stone),
            ((3, 2, 0), stone),
        ]);
        let world = LiveWorldPathAdapter::with_block_getter(
            crate::worldgen::DimId::Overworld,
            reg,
            |dim, x, y, z| {
                assert_eq!(dim, crate::worldgen::DimId::Overworld);
                blocks
                    .get(&(x, y, z))
                    .copied()
                    .unwrap_or(if y == 0 { stone } else { air })
            },
        );

        assert_eq!((world.min_y(), world.max_y()), (-64, 319));
        assert_eq!(world.path_type(1, 1, 0), BlockPathType::Fence);
        assert_eq!(world.path_type(2, 1, 0), BlockPathType::Lava);
        assert_eq!(world.path_type(3, 1, 0), BlockPathType::Blocked);
        assert!(world.is_solid(1, 1, 0));
        assert!(world.collision_height(1, 1, 0) > 1.0);
        assert!(world.collision_height(3, 1, 0) > 0.0);
        assert_eq!(world.collision_height(2, 1, 0), 0.0);
        assert_eq!(world.path_type(0, 320, 0), BlockPathType::Open);
        assert!(!world.is_solid(0, 320, 0));
        assert_eq!(world.collision_height(0, 320, 0), 0.0);

        let path = PathFinder::default().find_path(&world, (0, 1, 0), (4, 1, 0));
        assert!(path.reached, "{path:?}");
        assert!(path.nodes.iter().any(|node| node.z != 0));
        assert!(path.nodes.iter().all(|node| {
            !matches!(node.path_type, BlockPathType::Fence | BlockPathType::Lava)
                && !(node.z == 0 && (1..=3).contains(&node.x))
        }));

        for dim in [crate::worldgen::DimId::Nether, crate::worldgen::DimId::End] {
            let live = LiveWorldPathAdapter::new(dim, reg);
            assert_eq!((live.min_y(), live.max_y()), (0, 255));
        }
    }

    #[test]
    fn mob_goal_factory_preserves_vanilla_priorities_and_tie_order() {
        use MobGoalKind::*;
        let cases: [(_, &[_], &[_], &[_]); 4] = [
            (
                "zombie",
                &[
                    (2, ZombieAttack),
                    (4, ZombieAttackTurtleEgg),
                    (
                        6,
                        MoveThroughVillage {
                            can_break_doors: false,
                        },
                    ),
                    (7, WaterAvoidingRandomStroll),
                    (8, LookAtPlayer),
                    (8, RandomLookAround),
                ],
                &[
                    (
                        1,
                        "HurtByTargetGoal (alerts allies, including zombified piglins)",
                    ),
                    (2, "NearestAttackableTargetGoal<Player>"),
                    (3, "NearestAttackableTargetGoal<AbstractVillager>"),
                    (3, "NearestAttackableTargetGoal<IronGolem>"),
                    (5, "NearestAttackableTargetGoal<Turtle> (baby on land)"),
                ],
                &[2, 4, 6, 7, 8, 8],
            ),
            (
                "skeleton",
                &[
                    (2, RestrictSun),
                    (3, FleeSun),
                    (3, AvoidEntity(AVOID_WOLF)),
                    (4, RangedBow { tipped: None }),
                    (5, WaterAvoidingRandomStroll),
                    (6, LookAtPlayer),
                    (6, RandomLookAround),
                ],
                &[
                    (1, "HurtByTargetGoal"),
                    (2, "NearestAttackableTargetGoal<Player>"),
                    (3, "NearestAttackableTargetGoal<IronGolem>"),
                    (3, "NearestAttackableTargetGoal<Turtle> (baby on land)"),
                ],
                &[2, 3, 3, 4, 5, 6, 6],
            ),
            (
                "creeper",
                &[
                    (1, Float),
                    (2, Swell),
                    (3, AvoidEntity(AVOID_OCELOT)),
                    (3, AvoidEntity(AVOID_CAT)),
                    (4, MeleeAttack),
                    (5, WaterAvoidingRandomStroll),
                    (6, LookAtPlayer),
                    (6, RandomLookAround),
                ],
                &[
                    (1, "NearestAttackableTargetGoal<Player>"),
                    (2, "HurtByTargetGoal"),
                ],
                &[1, 2, 3, 3, 4, 5, 6, 6],
            ),
            (
                "spider",
                &[
                    (1, Float),
                    (2, AvoidEntity(AVOID_UNROLLED_ARMADILLO)),
                    (3, LeapAtTarget),
                    (4, SpiderAttack),
                    (5, WaterAvoidingRandomStroll),
                    (6, LookAtPlayer),
                    (6, RandomLookAround),
                ],
                &[
                    (1, "HurtByTargetGoal"),
                    (2, "SpiderTargetGoal<Player>"),
                    (3, "SpiderTargetGoal<IronGolem>"),
                ],
                &[1, 2, 3, 4, 5, 6, 6],
            ),
        ];
        for (name, goals, missing_targets, active_priorities) in cases {
            let etype = entity_spec_by_name(name).unwrap().id;
            let plan = MobGoalPlan::for_entity(
                etype,
                MobGoalSettings {
                    skeleton_has_bow: true,
                    zombie_can_break_doors: false,
                },
            )
            .unwrap();
            assert_eq!(plan.goals, goals, "{name}");
            assert_eq!(plan.missing_targets, missing_targets, "{name}");
            let selector = plan.build_selector(0);
            let installed: Vec<_> = selector.goals.iter().map(|g| g.goal.priority()).collect();
            assert_eq!(installed, active_priorities, "{name}");
        }
    }

    #[test]
    fn mob_goal_factory_selects_skeleton_weapon_and_zombie_door_variant() {
        let entity_id = |name| entity_spec_by_name(name).unwrap().id;
        let unarmed =
            MobGoalPlan::for_entity(entity_id("skeleton"), MobGoalSettings::default()).unwrap();
        assert_eq!(unarmed.goals[3], (4, MobGoalKind::MeleeAttack));
        // The priority-2 sun restriction now installs ahead of everything.
        assert_eq!(unarmed.build_selector(0).goals[0].goal.priority(), 2);
        let breaking = MobGoalPlan::for_entity(
            entity_id("zombie"),
            MobGoalSettings {
                zombie_can_break_doors: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(breaking.goals[0], (1, MobGoalKind::BreakDoor));
        assert!(MobGoalPlan::for_entity(entity_id("cow"), MobGoalSettings::default()).is_none());
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn mob_goal_factories_and_tick_driver() {
        let mut creeper = build_goal_selector_for_etype(23);
        let mut zombie = build_goal_selector_for_etype(124);
        let mut cow = build_goal_selector_for_etype(22);
        assert_eq!(creeper.goals.len(), 8);
        // Float + BreakDoor + attack + egg stomp + village + stroll + look +
        // look-around — the live schedule registers every vanilla goal.
        assert_eq!(zombie.goals.len(), 8);
        assert_eq!(cow.goals.len(), 6);
        assert_eq!(build_goal_selector_for_etype(-1).goals.len(), 0);
        assert_eq!(build_goal_selector_for_etype(128).goals.len(), 0);
        assert_eq!(build_goal_selector_for_etype(53).goals.len(), 3);

        let close_target = GoalContext {
            position: (0.0, 1.0, 0.0),
            target: Some(target(15, 2.0, 1.0, 0.0)),
            ..GoalContext::default()
        };
        assert_eq!(creeper.tick(&close_target).swell, Some(true));
        let close_target = GoalContext {
            target: Some(target(15, 1.0, 0.0, 0.0)),
            ..GoalContext::default()
        };
        assert_eq!(zombie.tick(&close_target).attack_target, Some(15));

        let mut player = target(12, 5.0, 1.0, 0.0);
        player.held_item = Some(854);
        let tempted = GoalContext {
            position: (0.0, 1.0, 0.0),
            nearest_player: Some(player),
            ..GoalContext::default()
        };
        assert_eq!(cow.tick(&tempted).move_target, Some(player.position));

        let mut profile = MobAiProfile::new(124);
        assert_eq!(profile.etype, 124);
        let view = GoalContext {
            position: (0.0, 1.0, 0.0),
            target: Some(target(42, 5.0, 1.0, 0.0)),
            ..GoalContext::default()
        };
        let step = tick_mob_ai_step(&mut profile, &view);
        assert_eq!(step.move_target, Some((5.0, 1.0, 0.0)));
        assert_eq!(step.movement_delta, (0.25, 0.0, 0.0));
        assert_eq!(step.look_yaw, Some(-90.0));
        assert!(step.look_pitch.is_some());
        assert_eq!(step.attack_target, None);

        let stone = state("stone", &[]);
        let slab = state("oak_slab", &[("type", "bottom")]);
        let world = LiveWorldPathAdapter::with_bounds(
            crate::block_registry::breg().unwrap(),
            0,
            4,
            |x, y, z| match (x, y, z) {
                (1, 0, 0) => slab,
                (_, 0, _) => stone,
                _ => 0,
            },
        );
        let view = GoalContext {
            position: (0.5, 1.0, 0.5),
            target: Some(target(42, 4.5, 1.0, 0.5)),
            world: Some(&world),
            ..GoalContext::default()
        };
        let step = tick_mob_ai_step(&mut profile, &view);
        assert_eq!(step.movement_delta, (0.25, 0.0, 0.0));
        assert_eq!(
            profile.navigation.next_waypoint(&world),
            Some((1.5, 0.5, 0.5))
        );
        let view = GoalContext {
            position: (1.5, 0.5, 0.5),
            ..view
        };
        let step = tick_mob_ai_step(&mut profile, &view);
        assert_eq!(step.movement_delta, (0.25, 0.0, 0.0));
        assert_eq!(
            profile.navigation.next_waypoint(&world),
            Some((2.5, 1.0, 0.5))
        );
    }

    #[test]
    fn path_type_default_penalties_and_clearance() {
        use BlockPathType::*;
        let all = [
            Blocked,
            Open,
            Walkable,
            WalkableDoor,
            Trapdoor,
            PowderSnow,
            DangerPowdersnow,
            Fence,
            Lava,
            Water,
            WaterBorder,
            Rail,
            UnpassableRail,
            DangerFire,
            DamageFire,
            DangerOther,
            DamageOther,
            DoorOpen,
            DoorWoodClosed,
            DoorIronClosed,
            Breach,
            Leaves,
            StickyHoney,
            Cocoa,
            DamageCautious,
            DangerTrapdoor,
        ];
        for ty in all {
            let expected = match ty {
                Blocked | PowderSnow | Fence | Lava | UnpassableRail | DamageOther
                | DoorWoodClosed | DoorIronClosed | Leaves => -1.0,
                Water | WaterBorder | DangerFire | DangerOther | StickyHoney => 8.0,
                DamageFire => 16.0,
                Breach => 4.0,
                _ => 0.0,
            };
            assert_eq!(default_malus(ty), expected, "{ty:?}");
        }
        let mut world = GridWorld::default();
        world.heights.insert((0, 0, 0), 0.5);
        world.set(0, 0, 0, Open);
        assert!(WalkNodeEvaluator::default().standable(&world, (0, 1, 0)));
        world.set(0, 2, 0, Blocked);
        assert!(!WalkNodeEvaluator::default().standable(&world, (0, 1, 0)));
        assert!(!WalkNodeEvaluator::default().standable(&world, (0, 321, 0)));
    }

    #[test]
    fn astar_detours_around_walls_and_maze_deterministically() {
        let mut world = GridWorld::default();
        for z in -1..=1 {
            world.wall(2, z, 2);
        }
        for x in 3..=6 {
            if x != 5 {
                world.wall(x, 2, 2);
            }
        }
        let finder = PathFinder::default();
        let path = finder.find_path(&world, (0, 1, 0), (7, 1, 0));
        assert!(path.reached, "{path:?}");
        assert_eq!(path.nodes.first().map(|n| (n.x, n.y, n.z)), Some((0, 1, 0)));
        assert_eq!(path.nodes.last().map(|n| (n.x, n.y, n.z)), Some((7, 1, 0)));
        assert!(path
            .nodes
            .iter()
            .all(|node| world.path_type(node.x, node.y, node.z) != BlockPathType::Blocked));
        assert!(path.nodes.iter().any(|node| node.z.abs() > 1));
        for _ in 0..4 {
            assert_eq!(
                positions(&finder.find_path(&world, (0, 1, 0), (7, 1, 0))),
                positions(&path)
            );
        }
    }

    #[test]
    fn astar_returns_closest_partial_path_when_enclosed_or_budget_exhausted() {
        let mut world = GridWorld::default();
        for dx in -1..=1 {
            for dz in -1..=1 {
                if dx != 0 || dz != 0 {
                    world.wall(dx, dz, 2);
                }
            }
        }
        let finder = PathFinder::default();
        let path = finder.find_path(&world, (0, 1, 0), (8, 1, 0));
        assert!(!path.reached);
        assert_eq!(positions(&path), vec![(0, 1, 0)]);
        assert_eq!(path.target, (8, 1, 0));

        let capped = PathFinder {
            max_visited_nodes: 3,
            ..PathFinder::default()
        };
        let partial = capped.find_path(&GridWorld::default(), (0, 1, 0), (20, 1, 0));
        assert!(!partial.reached);
        assert!(partial.nodes.last().is_some_and(|node| node.x > 0));
        assert!(partial.nodes.len() <= 3);
        assert_eq!(
            positions(&partial),
            positions(&capped.find_path(&GridWorld::default(), (0, 1, 0), (20, 1, 0)))
        );
        assert!(finder
            .find_path(&world, (0, 0, 0), (1, 1, 0))
            .nodes
            .is_empty());
    }

    #[test]
    fn diagonal_requires_both_cardinal_neighbor_clearances() {
        for blocked in [(1, 0), (0, 1)] {
            let mut world = GridWorld::default();
            world.wall(blocked.0, blocked.1, 2);
            let neighbors = WalkNodeEvaluator::default().neighbors(&world, (0, 1, 0));
            assert!(
                neighbors.iter().all(|n| (n.x, n.z) != (1, 1)),
                "blocked {blocked:?}"
            );
            let path = PathFinder::default().find_path(&world, (0, 1, 0), (1, 1, 1));
            assert!(path.reached);
            assert!(path.nodes.len() >= 3);
        }
    }

    #[test]
    fn step_up_headroom_and_two_block_obstacle() {
        let mut world = GridWorld::default();
        world.wall(1, 0, 1);
        let evaluator = WalkNodeEvaluator::default();
        let step = evaluator.neighbor(&world, (0, 1, 0), 1, 0).unwrap();
        assert_eq!((step.x, step.y, step.z), (1, 2, 0));
        assert!(
            PathFinder::default()
                .find_path(&world, (0, 1, 0), (1, 2, 0))
                .reached
        );
        world.wall(1, 0, 2);
        assert!(evaluator.neighbor(&world, (0, 1, 0), 1, 0).is_none());
        world.set(1, 2, 0, BlockPathType::Open);
        world.set(0, 3, 0, BlockPathType::Blocked);
        assert!(evaluator.neighbor(&world, (0, 1, 0), 1, 0).is_none());
    }

    #[test]
    fn safe_three_block_fall_four_block_drop_and_lava_pit() {
        let mut world = GridWorld::default();
        world.set(0, 3, 0, BlockPathType::Blocked);
        let evaluator = WalkNodeEvaluator::default();
        assert_eq!(evaluator.neighbor(&world, (0, 4, 0), 1, 0).unwrap().y, 1);
        assert!(
            PathFinder::default()
                .find_path(&world, (0, 4, 0), (1, 1, 0))
                .reached
        );
        world.set(0, 4, 0, BlockPathType::Blocked);
        assert!(evaluator.neighbor(&world, (0, 5, 0), 1, 0).is_none());

        let mut lava_world = GridWorld::default();
        lava_world.set(1, 1, 0, BlockPathType::Lava);
        assert!(evaluator.neighbor(&lava_world, (0, 1, 0), 1, 0).is_none());
        assert!(PathFinder::default()
            .find_path(&lava_world, (0, 1, 0), (1, 1, 0))
            .nodes
            .iter()
            .all(|node| node.path_type != BlockPathType::Lava));
    }

    #[test]
    fn water_malus_favors_dry_detour_but_can_cross_water() {
        let mut world = GridWorld::default();
        for x in 1..=5 {
            world.set(x, 1, 0, BlockPathType::Water);
        }
        let finder = PathFinder::default();
        let path = finder.find_path(&world, (0, 1, 0), (6, 1, 0));
        assert!(path.reached);
        assert!(path
            .nodes
            .iter()
            .all(|node| node.path_type != BlockPathType::Water));
        assert!(path.nodes.iter().any(|node| node.z != 0));

        for x in -20..=20 {
            world.wall(x, -1, 2);
            world.wall(x, 1, 2);
        }
        let wet = finder.find_path(&world, (0, 1, 0), (6, 1, 0));
        assert!(wet.reached);
        assert!(wet
            .nodes
            .iter()
            .any(|node| node.path_type == BlockPathType::Water));
    }

    #[test]
    fn path_navigation_advances_and_stops() {
        let world = GridWorld::default();
        let mut nav = PathNavigation::default();
        assert!(nav.is_done());
        assert!(nav.navigate_to(&PathFinder::default(), &world, (0, 1, 0), (2, 1, 0)));
        assert_eq!(nav.path().unwrap().nodes.len(), 3);
        assert_eq!(nav.next_waypoint(&world), Some((1.5, 1.0, 0.5)));
        nav.advance_if_reached(&world, (1.5, 1.0, 0.5), 0.1);
        assert_eq!(nav.next_waypoint(&world), Some((2.5, 1.0, 0.5)));
        nav.advance_if_reached(&world, (2.5, 1.0, 0.5), 0.1);
        assert!(nav.is_done());
        nav.stop();
        assert!(nav.path().is_none());
    }

    struct TracedGoal {
        id: &'static str,
        priority: i32,
        flags: u8,
        interruptable: bool,
        needs_fire: bool,
        events: Rc<RefCell<Vec<String>>>,
    }

    impl Goal for TracedGoal {
        fn priority(&self) -> i32 {
            self.priority
        }
        fn flags(&self) -> u8 {
            self.flags
        }
        fn can_use(&self, ctx: &GoalContext<'_>) -> bool {
            self.events.borrow_mut().push(format!("{} can_use", self.id));
            !self.needs_fire || ctx.on_fire
        }
        fn can_continue_to_use(&self, ctx: &GoalContext<'_>) -> bool {
            self.can_use(ctx)
        }
        fn is_interruptable(&self) -> bool {
            self.interruptable
        }
        fn start(&mut self, _: &GoalContext<'_>) {
            self.events.borrow_mut().push(format!("{} start", self.id));
        }
        fn tick(&mut self, _: &GoalContext<'_>) -> Option<GoalOutput> {
            self.events.borrow_mut().push(format!("{} tick", self.id));
            Some(GoalOutput {
                move_target: ((self.flags & GoalFlags::MOVE) != 0).then_some((5.0, 1.0, 0.0)),
                look_target: ((self.flags & GoalFlags::LOOK) != 0).then_some((3.0, 1.0, 0.0)),
                ..GoalOutput::default()
            })
        }
        fn stop(&mut self, _: &GoalContext<'_>) {
            self.events.borrow_mut().push(format!("{} stop", self.id));
        }
    }

    fn traced(
        id: &'static str,
        priority: i32,
        flags: u8,
        interruptable: bool,
        needs_fire: bool,
        events: &Rc<RefCell<Vec<String>>>,
    ) -> Box<dyn Goal> {
        Box::new(TracedGoal {
            id,
            priority,
            flags,
            interruptable,
            needs_fire,
            events: events.clone(),
        })
    }

    #[test]
    fn selector_preempts_move_and_keeps_independent_look_running() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut selector = GoalSelector::default();
        selector.add_goal(traced("low", 8, GoalFlags::MOVE, true, false, &events));
        selector.add_goal(traced("look", 4, GoalFlags::LOOK, true, false, &events));
        selector.add_goal(traced("high", 1, GoalFlags::MOVE, true, true, &events));
        let mut ctx = GoalContext::default();
        let first = selector.tick(&ctx);
        assert_eq!(first.move_target, Some((5.0, 1.0, 0.0)));
        assert_eq!(first.look_target, Some((3.0, 1.0, 0.0)));
        assert_eq!(selector.running_count(), 2);
        ctx.on_fire = true;
        selector.tick(&ctx);
        let record = events.borrow();
        let stopped = record.iter().position(|s| s == "low stop").unwrap();
        let started = record.iter().position(|s| s == "high start").unwrap();
        assert!(stopped < started);
        assert_eq!(record.iter().filter(|s| *s == "look start").count(), 1);
        drop(record);
        ctx.on_fire = false;
        selector.tick(&ctx);
        assert_eq!(selector.running_count(), 2);
        assert_eq!(
            events.borrow().iter().filter(|s| *s == "low start").count(),
            2
        );
        selector.stop_all(&ctx);
        assert_eq!(selector.running_count(), 0);
    }

    #[test]
    fn selector_does_not_evaluate_blocked_goal_can_use() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut selector = GoalSelector::default();
        selector.add_goal(traced("blocking", 0, GoalFlags::MOVE, true, true, &events));
        selector.add_goal(traced("waiting", 1, GoalFlags::MOVE, true, false, &events));
        let mut ctx = GoalContext {
            on_fire: true,
            ..GoalContext::default()
        };
        selector.tick(&ctx);
        assert!(!events.borrow().iter().any(|event| event == "waiting can_use"));
        ctx.on_fire = false;
        selector.tick(&ctx);
        assert!(events.borrow().iter().any(|event| event == "waiting start"));
    }

    #[test]
    fn selector_protects_noninterruptible_goal_and_separates_target_selector() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut selector = GoalSelector::default();
        selector.add_goal(traced("low", 9, GoalFlags::MOVE, false, false, &events));
        selector.add_goal(traced("high", 0, GoalFlags::MOVE, true, true, &events));
        let mut ctx = GoalContext::default();
        selector.tick(&ctx);
        ctx.on_fire = true;
        selector.tick(&ctx);
        assert_eq!(selector.running_count(), 1);
        assert!(!events.borrow().iter().any(|s| s == "high start"));

        let mut targets = TargetSelector::default();
        targets.add_goal(traced("target", 0, GoalFlags::TARGET, true, false, &events));
        targets.tick(&ctx);
        assert_eq!(targets.running_count(), 1);
        targets.stop_all(&ctx);
        assert_eq!(targets.running_count(), 0);
    }

    #[test]
    fn swell_goal_ignites_inside_three_and_disarms_after_seven_or_los_loss() {
        let mut selector = GoalSelector::default();
        selector.add_goal(Box::new(SwellGoal::new(2)));
        let mut ctx = GoalContext {
            target: Some(target(11, 2.0, 0.0, 0.0)),
            ..GoalContext::default()
        };
        assert_eq!(selector.tick(&ctx).swell, Some(true));
        ctx.target = Some(target(11, 7.0, 0.0, 0.0));
        assert_eq!(selector.tick(&ctx).swell, Some(true));
        ctx.target = Some(target(11, 7.01, 0.0, 0.0));
        assert_eq!(selector.tick(&ctx).swell, Some(false));
        assert_eq!(selector.running_count(), 1);
        selector.tick(&ctx);
        assert_eq!(selector.running_count(), 0);
        ctx.target = Some(target(11, 2.0, 0.0, 0.0));
        assert_eq!(selector.tick(&ctx).swell, Some(true));
        let mut hidden = target(11, 1.0, 0.0, 0.0);
        hidden.line_of_sight = false;
        ctx.target = Some(hidden);
        assert_eq!(selector.tick(&ctx).swell, Some(false));
        ctx.target = Some(target(11, 2.0, 0.0, 0.0));
        assert_eq!(selector.tick(&ctx).swell, Some(true));
        assert_eq!(selector.stop_all(&ctx).swell, Some(false));
        assert_eq!(selector.running_count(), 0);
        assert!(!SwellGoal::new(0).can_use(&GoalContext {
            target: Some(target(11, 3.0, 0.0, 0.0)),
            ..GoalContext::default()
        }));
    }

    #[test]
    fn melee_attack_waits_twenty_game_ticks_and_checks_reach() {
        let mut selector = GoalSelector::default();
        selector.add_goal(Box::new(MeleeAttackGoal::new(3)));
        let mut ctx = GoalContext {
            target: Some(target(7, 1.0, 0.0, 0.0)),
            ..GoalContext::default()
        };
        assert_eq!(selector.tick(&ctx).attack_target, Some(7));
        for _ in 0..19 {
            assert_eq!(selector.tick(&ctx).attack_target, None);
        }
        assert_eq!(selector.tick(&ctx).attack_target, Some(7));
        ctx.target = Some(target(7, 1.5, 0.0, 0.0));
        for _ in 0..20 {
            assert_eq!(selector.tick(&ctx).attack_target, None);
        }
        assert_eq!(selector.tick(&ctx).move_target, Some((1.5, 0.0, 0.0)));
        ctx.target = Some(target(7, 1.0, 0.0, 0.0));
        assert_eq!(selector.tick(&ctx).attack_target, Some(7));
    }

    #[test]
    fn float_and_panic_goals_respect_fluid_and_safe_fleeing() {
        let mut float = FloatGoal { priority: 0 };
        let mut ctx = GoalContext::default();
        assert!(!float.can_use(&ctx));
        ctx.in_water = true;
        assert!(float.can_use(&ctx));
        assert!(float.tick(&ctx).unwrap().jump);
        ctx.in_water = false;
        ctx.in_lava = true;
        assert!(float.can_continue_to_use(&ctx));

        let mut world = GridWorld::default();
        world.set(5, 1, 0, BlockPathType::Water);
        world.set(-6, 1, 0, BlockPathType::DangerFire);
        let mut panic = PanicGoal::new(1);
        let on_fire = GoalContext {
            position: (0.5, 1.0, 0.5),
            on_fire: true,
            world: Some(&world),
            ..GoalContext::default()
        };
        assert!(panic.can_use(&on_fire));
        panic.start(&on_fire);
        assert_eq!(
            panic.tick(&on_fire).unwrap().move_target,
            Some((5.5, 1.0, 0.5))
        );
        panic.stop(&on_fire);
        assert!(panic.destination.is_none());
        let hurt = GoalContext {
            recently_hurt: true,
            target: Some(target(20, -2.0, 1.0, 0.0)),
            ..GoalContext::default()
        };
        panic.start(&hurt);
        assert!(panic.tick(&hurt).unwrap().move_target.unwrap().0 > hurt.position.0);
    }

    #[test]
    fn temptation_requires_held_item_and_observes_calm_down() {
        let mut selector = GoalSelector::default();
        selector.add_goal(Box::new(TemptGoal::new(3, 42)));
        let mut player = target(8, 5.0, 1.0, 0.0);
        player.held_item = Some(42);
        let mut ctx = GoalContext {
            nearest_player: Some(player),
            ..GoalContext::default()
        };
        assert_eq!(selector.tick(&ctx).move_target, Some(player.position));
        ctx.nearest_player = None;
        selector.tick(&ctx);
        assert_eq!(selector.running_count(), 0);
        ctx.nearest_player = Some(player);
        for _ in 0..98 {
            assert!(selector.tick(&ctx).move_target.is_none());
        }
        assert_eq!(selector.tick(&ctx).move_target, Some(player.position));
        let mut different = TemptGoal::new(1, 9);
        different.calm_down_on_interrupt = false;
        assert!(!different.can_use(&ctx));
    }

    #[test]
    fn breeding_requires_love_compatibility_and_sixty_close_ticks() {
        let mut selector = GoalSelector::default();
        selector.add_goal(Box::new(BreedGoal::new(4)));
        let mut mate = target(16, 2.0, 0.0, 0.0);
        mate.compatible = true;
        mate.in_love = true;
        let mut ctx = GoalContext {
            mate: Some(mate),
            in_love: true,
            ..GoalContext::default()
        };
        for _ in 0..59 {
            assert_eq!(selector.tick(&ctx).breed_target, None);
        }
        assert_eq!(selector.tick(&ctx).breed_target, Some(16));
        selector.tick(&ctx);
        assert_eq!(selector.running_count(), 0);
        ctx.mate = Some(AiEntityPerception {
            compatible: false,
            ..mate
        });
        assert!(!BreedGoal::new(4).can_use(&ctx));
        ctx.in_love = false;
        selector.tick(&ctx);
        ctx.in_love = true;
        ctx.mate = Some(mate);
        assert_eq!(selector.tick(&ctx).breed_target, None);
        assert_eq!(selector.running_count(), 1);
    }

    #[test]
    fn seeded_stroll_prefers_dry_walkable_locations_and_look_goal_tracks_player() {
        let mut world = GridWorld::default();
        world.set(3, 1, 0, BlockPathType::Water);
        let ctx = GoalContext {
            position: (0.5, 1.0, 0.5),
            world: Some(&world),
            ..GoalContext::default()
        };
        let mut first = WaterAvoidingRandomStrollGoal::new(8, 44);
        let mut second = WaterAvoidingRandomStrollGoal::new(8, 44);
        first.start(&ctx);
        second.start(&ctx);
        assert_eq!(first.tick(&ctx), second.tick(&ctx));
        let p = first.tick(&ctx).unwrap().move_target.unwrap();
        assert!(matches!(
            world.path_type(p.0.floor() as i32, p.1.floor() as i32, p.2.floor() as i32),
            BlockPathType::Walkable | BlockPathType::Open
        ));
        first.stop(&ctx);
        assert!(first.destination.is_none());
        let mut selector = GoalSelector::default();
        selector.add_goal(Box::new(LookAtPlayerGoal {
            priority: 5,
            look_distance: 10.0,
        }));
        let view = GoalContext {
            nearest_player: Some(target(99, 3.0, 1.0, 0.0)),
            ..GoalContext::default()
        };
        assert_eq!(selector.tick(&view).look_target, Some((3.0, 1.0, 0.0)));
        assert_eq!(selector.tick(&GoalContext::default()).look_target, None);
        assert_eq!(selector.running_count(), 0);
    }

    struct CrowdedWorld(Option<(usize, usize, u32)>);

    impl PathWorld for CrowdedWorld {
        fn path_type(&self, _: i32, _: i32, _: i32) -> BlockPathType {
            BlockPathType::Open
        }
        fn is_solid(&self, _: i32, _: i32, _: i32) -> bool {
            false
        }
        fn collision_height(&self, _: i32, _: i32, _: i32) -> f64 {
            0.0
        }
        fn min_y(&self) -> i32 {
            -64
        }
        fn max_y(&self) -> i32 {
            320
        }
        fn cramming_census(&self, _: f64, _: f64, _: f64) -> Option<(usize, usize, u32)> {
            self.0
        }
    }

    #[test]
    fn v1211_veh_ai_creeper_fuse_and_abort_run_through_live_mob_ai_step() {
        let mut profile = MobAiProfile::new(23);
        let near = GoalContext {
            position: (0.0, 1.0, 0.0),
            target: Some(target(15, 1.0, 1.0, 0.0)),
            ..GoalContext::default()
        };
        // SwellGoal reports the fuse state the live creeper tick consumes.
        assert_eq!(tick_mob_ai_step(&mut profile, &near).swell, Some(true));
        assert_eq!(tick_mob_ai_step(&mut profile, &near).swell, Some(true));
        let far = GoalContext {
            position: (0.0, 1.0, 0.0),
            target: Some(target(15, 30.0, 1.0, 0.0)),
            ..GoalContext::default()
        };
        // Losing the target aborts the lit fuse through the same step result.
        assert_eq!(tick_mob_ai_step(&mut profile, &far).swell, Some(false));
    }

    #[test]
    fn v1211_veh_ai_spider_leap_runs_through_live_mob_ai_step() {
        let mut profile = MobAiProfile::new(100);
        let ctx = GoalContext {
            position: (0.0, 1.0, 0.0),
            target: Some(target(7, 2.0, 1.0, 0.0)),
            ..GoalContext::default()
        };
        // The vanilla 1-in-5 idle roll arms the pounce; the deterministic
        // seed stream lands it inside a bounded tick window.
        let mut leap_tick = None;
        for tick in 1..=200 {
            let step = tick_mob_ai_step(&mut profile, &ctx);
            if step.jump && step.movement_delta == (0.4, 0.0, 0.0) {
                leap_tick = Some(tick);
                break;
            }
        }
        assert!(leap_tick.is_some(), "LeapAtTargetGoal never armed");
        // The impulse re-applies for the remaining pounce ticks.
        for _ in 0..2 {
            assert_eq!(
                tick_mob_ai_step(&mut profile, &ctx).movement_delta,
                (0.4, 0.0, 0.0)
            );
        }
        // Once the impulse ends the melee chase stride returns.
        let mut stride = None;
        for _ in 0..60 {
            let step = tick_mob_ai_step(&mut profile, &ctx);
            if step.movement_delta != (0.4, 0.0, 0.0) {
                stride = Some(step);
                break;
            }
        }
        assert_eq!(stride.unwrap().movement_delta, (0.25, 0.0, 0.0));
    }

    #[test]
    fn v1211_veh_ai_skeleton_strafe_and_bow_draw_run_through_live_step() {
        let mut profile = MobAiProfile::new(91);
        let ctx = GoalContext {
            position: (0.0, 1.0, 0.0),
            target: Some(target(9, 12.0, 1.0, 0.0)),
            ..GoalContext::default()
        };
        // Vanilla draws on first sight; the first arrow looses once the bow
        // finishes its 20-tick pull.
        let mut first_shot = None;
        let mut step = MobAiStepResult::default();
        for tick in 1..=40 {
            step = tick_mob_ai_step(&mut profile, &ctx);
            if step.attack_target.is_some() {
                first_shot = Some(tick);
                break;
            }
        }
        assert_eq!(first_shot, Some(20));
        assert_eq!(step.attack_target, Some(9));
        let RangedProjectileAction::Arrow {
            target_eid,
            velocity,
            tipped,
            ..
        } = step.ranged_attack.expect("the release emits an arrow")
        else {
            panic!("expected an arrow launch");
        };
        assert_eq!(target_eid, 9);
        assert_eq!(tipped, None);
        let speed = (velocity.0 * velocity.0 + velocity.1 * velocity.1 + velocity.2 * velocity.2)
            .sqrt();
        assert!((speed - 1.6).abs() < 1.0e-9, "arrow speed {speed}");
        // Once the target has been seen for 20 ticks the skeleton holds and
        // strafes inside the band (skeleton_strafe_velocity, clockwise off).
        let strafe = tick_mob_ai_step(&mut profile, &ctx);
        assert_eq!(strafe.movement_delta, (-0.25, 0.0, 0.25));
        // The next release lands once the attack interval expires and the
        // bow redraws — but the LOOK-flagged random-look goal can preempt the
        // bow mid-cooldown, and vanilla `stop()` wipes `attackTime`, so the
        // exact tick shifts with the goal schedule. Pin the interval math in
        // `ranged_bow_attack_goal_releases_tipped_arrow_after_draw`; here we
        // only require another full draw cycle inside a bounded window.
        let interval = usize::from(skeleton_bow_charge_interval(
            DIFFICULTY.load(Ordering::Relaxed) >= 3,
        ));
        let mut second_shot = None;
        for tick in (first_shot.unwrap() + 1)..=160 {
            if tick_mob_ai_step(&mut profile, &ctx).attack_target.is_some() {
                second_shot = Some(tick);
                break;
            }
        }
        let second = second_shot.expect("bow never released a second arrow");
        let gap = second - first_shot.unwrap();
        assert!(
            (20..=interval + 60).contains(&gap),
            "second shot gap {gap} ticks"
        );
    }

    #[test]
    fn v1211_veh_ai_drowned_swim_chase_runs_through_live_mob_ai_step() {
        let mut profile = MobAiProfile::new(27);
        let ctx = GoalContext {
            position: (0.0, 1.0, 0.0),
            in_water: true,
            target: Some(target(11, 0.0, 1.0, 8.0)),
            ..GoalContext::default()
        };
        let step = tick_mob_ai_step(&mut profile, &ctx);
        // drowned_swim_chase_velocity: unit chase * 0.1 replaces the stride.
        assert_eq!(step.movement_delta, (0.0, 0.0, 0.1));
        assert!(step.jump);
        let dry = GoalContext {
            position: (0.0, 1.0, 0.0),
            target: Some(target(11, 0.0, 1.0, 8.0)),
            ..GoalContext::default()
        };
        let step = tick_mob_ai_step(&mut profile, &dry);
        assert_ne!(step.movement_delta, (0.0, 0.0, 0.1));
    }

    #[test]
    fn v1211_veh_ai_entity_cramming_runs_through_live_mob_ai_step() {
        let mut profile = MobAiProfile::new(124);
        let ctx = GoalContext {
            position: (0.5, 1.0, 0.5),
            world: Some(&CrowdedWorld(Some((100, 100, 0)))),
            ..GoalContext::default()
        };
        assert_eq!(tick_mob_ai_step(&mut profile, &ctx).cramming_damage, 6.0);
        let ctx = GoalContext {
            position: (0.5, 1.0, 0.5),
            world: Some(&CrowdedWorld(Some((100, 100, 1)))),
            ..GoalContext::default()
        };
        assert_eq!(tick_mob_ai_step(&mut profile, &ctx).cramming_damage, 0.0);
        let ctx = GoalContext {
            position: (0.5, 1.0, 0.5),
            world: Some(&CrowdedWorld(None)),
            ..GoalContext::default()
        };
        assert_eq!(tick_mob_ai_step(&mut profile, &ctx).cramming_damage, 0.0);
    }

    #[test]
    fn avoid_entity_goal_flees_within_max_dist_and_sprints_inside_seven() {
        let spec = AvoidEntitySpec {
            flee_etypes: &[99],
            require_unscared: false,
            max_dist: 10.0,
            walk_speed: 1.0,
            sprint_speed: 1.5,
        };
        let mut goal = AvoidEntityGoal::new(3, spec);
        let ctx = GoalContext {
            position: (0.0, 1.0, 0.0),
            ..GoalContext::default()
        };
        assert!(!goal.can_use(&ctx));
        let mut perception = MobPerception::default();
        perception.nearby_mobs.push(mob_at(99, 7, 8.0, 0.0));
        goal.perceive(ctx.position, &perception);
        assert!(goal.can_use(&ctx));
        let out = goal.tick(&ctx).unwrap();
        let (x, _, z) = out.move_target.unwrap();
        assert!(x < 0.0 && z == 0.0);
        // Eight blocks out is past the 7-block sprint band: walk modifier.
        assert_eq!(out.speed_modifier, Some(1.0));
        perception.nearby_mobs = vec![mob_at(99, 7, 6.0, 0.0)];
        goal.perceive(ctx.position, &perception);
        assert_eq!(goal.tick(&ctx).unwrap().speed_modifier, Some(1.5));
        // Outside max_dist the goal drops the threat.
        perception.nearby_mobs = vec![mob_at(99, 7, 10.5, 0.0)];
        goal.perceive(ctx.position, &perception);
        assert!(!goal.can_use(&ctx));
    }

    #[test]
    fn creeper_flees_cat_and_ocelot_and_aborts_swell() {
        let plan = MobGoalPlan::for_entity(23, MobGoalSettings::default()).unwrap();
        let mut selector = plan.build_selector(0);
        let mut ctx = GoalContext {
            position: (0.0, 1.0, 0.0),
            ..GoalContext::default()
        };
        // A cat inside 6 blocks sends the creeper the other way at sprint.
        selector.perception.nearby_mobs = vec![mob_at(15, 7, 3.0, 0.0)];
        let out = selector.tick(&ctx);
        let dest = out.move_target.expect("creeper must flee the cat");
        assert!(
            entity_distance_sq(dest, (3.0, 1.0, 0.0))
                > entity_distance_sq(ctx.position, (3.0, 1.0, 0.0)),
            "flee destination {dest:?} must be further from the cat"
        );
        assert!((out.speed_modifier.unwrap() - 1.2).abs() < 1.0e-6);
        // Ocelots scare creepers the same way.
        selector.perception.nearby_mobs = vec![mob_at(72, 7, -3.0, 0.0)];
        let out = selector.tick(&ctx);
        assert!(out.move_target.unwrap().0 > ctx.position.0);
        // Six blocks is the whole trigger radius.
        selector.perception.nearby_mobs = vec![mob_at(15, 7, 7.0, 0.0)];
        assert!(selector.tick(&ctx).move_target.is_none());
        // A lit fuse aborts the moment a cat enters the flee radius.
        selector.perception.nearby_mobs = Vec::new();
        ctx.target = Some(target(9, 2.0, 1.0, 0.0));
        assert_eq!(selector.tick(&ctx).swell, Some(true));
        selector.perception.nearby_mobs = vec![mob_at(15, 7, 1.0, 0.5)];
        let out = selector.tick(&ctx);
        assert_eq!(out.swell, Some(false));
        assert!(out.move_target.unwrap().0 < 0.0);
    }

    #[test]
    fn skeleton_family_flees_wolves_within_six_blocks() {
        for name in ["skeleton", "stray", "bogged"] {
            let etype = entity_spec_by_name(name).unwrap().id;
            let mut selector = build_goal_selector_for_etype(etype);
            let ctx = GoalContext {
                position: (0.0, 1.0, 0.0),
                ..GoalContext::default()
            };
            selector.perception.nearby_mobs = vec![mob_at(122, 7, 4.0, 0.0)];
            let out = selector.tick(&ctx);
            assert!(
                out.move_target.is_some_and(|(x, ..)| x < 0.0),
                "{name} must flee the wolf"
            );
            selector.perception.nearby_mobs = vec![mob_at(122, 7, 7.0, 0.0)];
            assert!(selector.tick(&ctx).move_target.is_none(), "{name}");
        }
    }

    #[test]
    fn spider_family_flees_only_unscared_armadillos() {
        for name in ["spider", "cave_spider"] {
            let etype = entity_spec_by_name(name).unwrap().id;
            let mut selector = build_goal_selector_for_etype(etype);
            let ctx = GoalContext {
                position: (0.0, 1.0, 0.0),
                ..GoalContext::default()
            };
            let mut armadillo = mob_at(2, 7, 4.0, 0.0);
            selector.perception.nearby_mobs = vec![armadillo];
            assert!(
                selector
                    .tick(&ctx)
                    .move_target
                    .is_some_and(|(x, ..)| x < 0.0),
                "{name} must flee the unrolled armadillo"
            );
            armadillo.scared = true;
            selector.perception.nearby_mobs = vec![armadillo];
            assert!(selector.tick(&ctx).move_target.is_none(), "{name}");
        }
    }

    #[test]
    fn v1211_veh_ai_creeper_flee_speed_applies_sprint_modifier_through_step() {
        let mut profile = MobAiProfile::new(23);
        profile.perception.nearby_mobs = vec![mob_at(15, 7, 3.0, 0.0)];
        let ctx = GoalContext {
            position: (0.0, 1.0, 0.0),
            ..GoalContext::default()
        };
        let step = tick_mob_ai_step(&mut profile, &ctx);
        // movement_speed 0.25 * sprint modifier 1.2, straight away from the cat.
        assert_eq!(step.move_target, Some((-6.0, 1.0, 0.0)));
        assert!((step.movement_delta.0 + 0.3).abs() < 1.0e-6);
        assert_eq!((step.movement_delta.1, step.movement_delta.2), (0.0, 0.0));
    }

    #[test]
    fn snow_golem_targets_enemies_and_throws_snowballs_on_cadence() {
        let mut profile = MobAiProfile::new(96);
        profile.perception.nearby_mobs = vec![mob_at(124, 7, 5.0, 0.0)];
        let ctx = GoalContext {
            position: (0.0, 1.0, 0.0),
            ..GoalContext::default()
        };
        assert_eq!(tick_mob_ai_step(&mut profile, &ctx).mob_target, Some(7));
        let mut shots = Vec::new();
        for tick in 2..=41 {
            let step = tick_mob_ai_step(&mut profile, &ctx);
            if let Some(RangedProjectileAction::Snowball {
                target_eid,
                target_pos,
                velocity,
            }) = step.ranged_attack
            {
                shots.push((tick, target_eid, target_pos, velocity));
            }
        }
        // Vanilla arms the 20-tick interval on first sight, then repeats it.
        assert_eq!(shots.len(), 2, "{shots:?}");
        assert_eq!(shots[0].0, 21);
        assert_eq!(shots[1].0, 41);
        assert!(shots.iter().all(|(_, eid, ..)| *eid == 7));
        let (vx, vy, vz) = shots[0].3;
        let speed = (vx * vx + vy * vy + vz * vz).sqrt();
        assert!((speed - 1.6).abs() < 1.0e-9, "snowball speed {speed}");
        assert!(vx > 0.0, "snowball flies toward the target");
        // Passive mobs are never Enemy targets.
        profile.perception.nearby_mobs = vec![mob_at(22, 8, 5.0, 0.0)];
        assert_eq!(tick_mob_ai_step(&mut profile, &ctx).mob_target, None);
    }

    #[test]
    fn iron_golem_targets_hostile_mobs_but_never_creepers() {
        let mut profile = MobAiProfile::new(57);
        let ctx = GoalContext {
            position: (0.0, 1.0, 0.0),
            ..GoalContext::default()
        };
        profile.perception.nearby_mobs = vec![mob_at(23, 5, 4.0, 0.0)];
        assert_eq!(tick_mob_ai_step(&mut profile, &ctx).mob_target, None);
        profile.perception.nearby_mobs = vec![mob_at(124, 7, 4.0, 0.0)];
        assert_eq!(tick_mob_ai_step(&mut profile, &ctx).mob_target, Some(7));
        // The creeper is the closest mob yet the skeleton still wins.
        profile.perception.nearby_mobs = vec![
            mob_at(124, 7, 10.0, 0.0),
            mob_at(91, 8, 3.0, 0.0),
            mob_at(23, 5, 1.0, 0.0),
        ];
        assert_eq!(tick_mob_ai_step(&mut profile, &ctx).mob_target, Some(8));
    }

    #[test]
    fn untamed_wolf_targets_sheep_and_skeletons() {
        let mut profile = MobAiProfile::new(122);
        let ctx = GoalContext {
            position: (0.0, 1.0, 0.0),
            ..GoalContext::default()
        };
        profile.perception.nearby_mobs = vec![mob_at(87, 7, 5.0, 0.0)];
        assert_eq!(tick_mob_ai_step(&mut profile, &ctx).mob_target, Some(7));
        profile.perception.nearby_mobs = vec![mob_at(91, 8, 5.0, 0.0)];
        assert_eq!(tick_mob_ai_step(&mut profile, &ctx).mob_target, Some(8));
        // Pigs are not wolf prey.
        profile.perception.nearby_mobs = vec![mob_at(77, 9, 5.0, 0.0)];
        assert_eq!(tick_mob_ai_step(&mut profile, &ctx).mob_target, None);
    }

    #[test]
    fn fox_targets_poultry_rabbits_fish_and_baby_turtles() {
        let mut profile = MobAiProfile::new(42);
        let ctx = GoalContext {
            position: (0.0, 1.0, 0.0),
            ..GoalContext::default()
        };
        for etype in [19, 84, 20, 86, 110] {
            profile.perception.nearby_mobs = vec![mob_at(etype, 7, 5.0, 0.0)];
            assert_eq!(
                tick_mob_ai_step(&mut profile, &ctx).mob_target,
                Some(7),
                "fox must target etype {etype}"
            );
        }
        // Turtles only qualify while babies.
        profile.perception.nearby_mobs = vec![mob_at(111, 7, 5.0, 0.0)];
        assert_eq!(tick_mob_ai_step(&mut profile, &ctx).mob_target, None);
        let mut baby = mob_at(111, 7, 5.0, 0.0);
        baby.baby = true;
        profile.perception.nearby_mobs = vec![baby];
        assert_eq!(tick_mob_ai_step(&mut profile, &ctx).mob_target, Some(7));
    }

    #[test]
    fn random_look_around_goal_arms_and_holds_a_level_look() {
        let ctx = GoalContext {
            position: (4.5, 1.0, 4.5),
            ..GoalContext::default()
        };
        let mut goal = RandomLookAroundGoal::new(8, 1.62, 7);
        assert!(!goal.can_use(&ctx));
        // The 2% roll lands deterministically inside a bounded idle window.
        let mut armed_at = None;
        for tick in 1..=200 {
            goal.idle_tick(&ctx);
            if goal.can_use(&ctx) {
                armed_at = Some(tick);
                break;
            }
        }
        assert!(armed_at.is_some(), "look-around never armed");
        goal.start(&ctx);
        let out = goal.tick(&ctx).unwrap();
        let (lx, ly, lz) = out.look_target.unwrap();
        // The look point sits on the eye line at unit distance in the XZ
        // plane (rel = (cos, sin) of a uniform angle).
        assert_eq!(ly, ctx.position.1 + 1.62);
        assert!(
            ((lx - ctx.position.0).powi(2) + (lz - ctx.position.2).powi(2) - 1.0).abs()
                < 1.0e-9
        );
        // `20 + nextInt(20)` look ticks: the goal keeps ticking while
        // `lookTime >= 0`, so the whole run lasts 21..40 ticks.
        let mut ticks = 1;
        while goal.can_continue_to_use(&ctx) {
            goal.tick(&ctx);
            ticks += 1;
        }
        assert!((21..=40).contains(&ticks), "look run lasted {ticks} ticks");
    }

    #[test]
    fn leap_at_target_goal_pounces_only_inside_the_band() {
        let mut goal = LeapAtTargetGoal::new(3, 11);
        let in_band = GoalContext {
            position: (0.0, 1.0, 0.0),
            target: Some(target(7, 3.0, 1.0, 0.0)),
            ..GoalContext::default()
        };
        let mut armed = false;
        for _ in 0..200 {
            goal.idle_tick(&in_band);
            if goal.can_use(&in_band) {
                armed = true;
                break;
            }
        }
        assert!(armed, "leap never armed inside [2, 4] blocks");
        goal.start(&in_band);
        let first = goal.tick(&in_band).unwrap();
        assert!(first.jump);
        let impulse = first.move_impulse.unwrap();
        let speed = impulse.0.hypot(impulse.1);
        assert!((speed - 0.4).abs() < 1.0e-9, "forward impulse {speed}");
        // The impulse re-applies for the remaining pounce ticks, then stops.
        for _ in 0..2 {
            assert!(goal.can_continue_to_use(&in_band));
            goal.tick(&in_band);
        }
        assert!(!goal.can_continue_to_use(&in_band));

        // Outside the band the roll can never arm.
        let mut out_of_band = LeapAtTargetGoal::new(3, 11);
        let far = GoalContext {
            position: (0.0, 1.0, 0.0),
            target: Some(target(7, 10.0, 1.0, 0.0)),
            ..GoalContext::default()
        };
        for _ in 0..200 {
            out_of_band.idle_tick(&far);
            assert!(!out_of_band.can_use(&far));
        }
    }

    #[test]
    fn spider_attack_goal_stays_passive_in_open_daylight() {
        let world = GridWorld::default();
        let mut goal = SpiderAttackGoal::new(4);
        goal.daytime = RuleProbe(|| true);
        let sunlit = GoalContext {
            position: (0.5, 1.0, 0.5),
            target: Some(target(5, 1.5, 1.0, 0.5)),
            world: Some(&world),
            ..GoalContext::default()
        };
        // Daylight under open sky: the unprovoked spider does not attack.
        assert!(!goal.can_use(&sunlit));
        // Provoked spiders still fight back.
        let hurt = GoalContext {
            recently_hurt: true,
            ..sunlit
        };
        assert!(goal.can_use(&hurt));
        // Night falls: the same spider attacks.
        goal.daytime = RuleProbe(|| false);
        assert!(goal.can_use(&sunlit));
    }

    #[test]
    fn zombie_attack_goal_raises_arms_while_engaged() {
        let mut goal = ZombieAttackGoal::new(2);
        let close = GoalContext {
            position: (0.0, 1.0, 0.0),
            target: Some(target(9, 1.0, 1.0, 0.0)),
            ..GoalContext::default()
        };
        goal.start(&close);
        let out = goal.tick(&close).unwrap();
        assert_eq!(out.arms_raised, Some(true));
        assert_eq!(out.attack_target, Some(9));
        let stop = goal.stop_output().unwrap();
        assert_eq!(stop.arms_raised, Some(false));
    }

    #[test]
    fn break_door_goal_cracks_then_destroys_wooden_door() {
        let mut world = GridWorld::default();
        world.set(0, 1, 1, BlockPathType::DoorWoodClosed);
        fn ctx_at(world: &GridWorld) -> GoalContext<'_> {
            GoalContext {
                position: (0.5, 1.0, 0.5),
                world: Some(world),
                ..GoalContext::default()
            }
        }
        let mut goal = BreakDoorGoal::new(1);
        goal.mob_griefing = RuleProbe(|| true);
        goal.difficulty_test = RuleProbe(|| true);
        assert!(goal.can_use(&ctx_at(&world)));
        goal.start(&ctx_at(&world));
        // Ten crack stages broadcast over the 240-tick break.
        let mut stages = Vec::new();
        let mut destroyed = None;
        for _ in 0..240 {
            let out = goal.tick(&ctx_at(&world)).unwrap();
            if let Some((cell, stage)) = out.door_break_progress {
                stages.push((cell, stage));
            }
            if let Some(cell) = out.destroy_block {
                destroyed = Some(cell);
            }
        }
        assert_eq!(destroyed, Some((0, 1, 1)));
        assert_eq!(stages.len(), 239);
        assert_eq!(stages[0], ((0, 1, 1), 0));
        assert_eq!(stages[239 - 1], ((0, 1, 1), 9));
        assert!(!goal.can_continue_to_use(&ctx_at(&world)));

        // A door that opens mid-break resets the progress.
        let mut reset = BreakDoorGoal::new(1);
        reset.mob_griefing = RuleProbe(|| true);
        reset.difficulty_test = RuleProbe(|| true);
        reset.start(&ctx_at(&world));
        reset.tick(&ctx_at(&world));
        world.set(0, 1, 1, BlockPathType::DoorOpen);
        assert!(!reset.can_continue_to_use(&ctx_at(&world)));
        world.set(0, 1, 1, BlockPathType::DoorWoodClosed);

        // mobGriefing off or peaceful/easy difficulty gates the whole goal.
        let mut gated = BreakDoorGoal::new(1);
        gated.mob_griefing = RuleProbe(|| false);
        gated.difficulty_test = RuleProbe(|| true);
        assert!(!gated.can_use(&ctx_at(&world)));
        gated.mob_griefing = RuleProbe(|| true);
        gated.difficulty_test = RuleProbe(|| false);
        assert!(!gated.can_use(&ctx_at(&world)));
    }

    #[test]
    fn turtle_egg_goal_seeks_and_stomps_the_nearest_egg() {
        let mut world = GridWorld::default();
        world.name(3, 1, 0, "minecraft:turtle_egg");
        world.name(9, 1, 0, "minecraft:turtle_egg");
        let start = GoalContext {
            position: (0.5, 1.0, 0.5),
            world: Some(&world),
            ..GoalContext::default()
        };
        let mut goal = ZombieAttackTurtleEggGoal::new(4, 3);
        goal.mob_griefing = RuleProbe(|| true);
        assert!(!goal.can_use(&start));
        goal.idle_tick(&start);
        assert!(goal.can_use(&start));
        goal.start(&start);
        // The closer egg wins and the mob paths onto the block above it.
        let out = goal.tick(&start).unwrap();
        assert_eq!(out.move_target, Some((3.5, 2.0, 0.5)));
        // Standing on the egg, the stomp counter removes it after 60 ticks.
        {
            let on_egg = GoalContext {
                position: (3.5, 2.0, 0.5),
                world: Some(&world),
                ..GoalContext::default()
            };
            let mut destroyed = None;
            let mut stomp_jumps = 0;
            for _ in 0..61 {
                let out = goal.tick(&on_egg).unwrap();
                if out.jump {
                    stomp_jumps += 1;
                }
                if let Some(cell) = out.destroy_block {
                    destroyed = Some(cell);
                }
            }
            // Vanilla hops at stomp ticks 20, 40 and 60 before the break.
            assert_eq!(stomp_jumps, 3);
            assert_eq!(destroyed, Some((3, 1, 0)));
        }
        // With the egg gone the goal drops out.
        world.name(3, 1, 0, "minecraft:air");
        let gone = GoalContext {
            position: (3.5, 2.0, 0.5),
            world: Some(&world),
            ..GoalContext::default()
        };
        assert!(!goal.can_continue_to_use(&gone));
        let gated_start = GoalContext {
            position: (0.5, 1.0, 0.5),
            world: Some(&world),
            ..GoalContext::default()
        };
        // mobGriefing=false blocks the whole behavior.
        let mut gated = ZombieAttackTurtleEggGoal::new(4, 3);
        gated.mob_griefing = RuleProbe(|| false);
        assert!(!gated.can_use(&gated_start));
    }

    #[test]
    fn sun_goals_restrict_and_flee_only_in_open_daylight() {
        let mut world = GridWorld::default();
        // A roof at y=3 shades the standable cell below it.
        world.set(3, 3, 0, BlockPathType::Blocked);
        let sunlit = GoalContext {
            position: (0.5, 1.0, 0.5),
            on_fire: true,
            world: Some(&world),
            ..GoalContext::default()
        };
        let mut restrict = RestrictSunGoal::new(2);
        restrict.daytime = RuleProbe(|| true);
        assert!(restrict.can_use(&sunlit));
        let out = restrict.tick(&sunlit).unwrap();
        assert_eq!(out.restrict_sun, Some(true));
        restrict.daytime = RuleProbe(|| false);
        assert!(!restrict.can_use(&sunlit));
        assert_eq!(restrict.stop_output().unwrap().restrict_sun, Some(false));

        let mut flee = FleeSunGoal::new(3);
        flee.daytime = RuleProbe(|| true);
        // Burning under open sky with shade nearby: the mob flees to the
        // shaded cell.
        assert!(flee.can_use(&sunlit));
        flee.start(&sunlit);
        let out = flee.tick(&sunlit).unwrap();
        assert_eq!(out.move_target, Some((3.5, 1.0, 0.5)));
        // Not burning or night: the goal never engages.
        let not_burning = GoalContext {
            on_fire: false,
            ..sunlit
        };
        assert!(!flee.can_use(&not_burning));
        flee.daytime = RuleProbe(|| false);
        assert!(!flee.can_use(&sunlit));
        // Reaching the shade ends the goal.
        flee.daytime = RuleProbe(|| true);
        let arrived = GoalContext {
            position: (3.5, 1.0, 0.5),
            ..sunlit
        };
        assert!(!flee.can_continue_to_use(&arrived));
    }

    #[test]
    fn move_through_village_goal_paths_to_doorways_at_night() {
        let mut world = GridWorld::default();
        world.set(8, 1, 0, BlockPathType::DoorOpen);
        world.set(12, 1, 4, BlockPathType::DoorWoodClosed);
        let ctx = GoalContext {
            position: (0.5, 1.0, 0.5),
            world: Some(&world),
            ..GoalContext::default()
        };
        let mut goal = MoveThroughVillageGoal::new(6, true, 5);
        goal.daytime = RuleProbe(|| false);
        goal.idle_tick(&ctx);
        assert!(goal.can_use(&ctx));
        goal.start(&ctx);
        // The open doorway wins over the farther closed door.
        assert_eq!(goal.tick(&ctx).unwrap().move_target, Some((8.5, 1.0, 0.5)));
        // Daylight ends the village movement entirely.
        goal.daytime = RuleProbe(|| true);
        assert!(!goal.can_use(&ctx));
        assert!(!goal.can_continue_to_use(&ctx));

        // Without door-breaking permission a closed wooden door is skipped.
        let mut no_break = MoveThroughVillageGoal::new(6, false, 5);
        no_break.daytime = RuleProbe(|| false);
        let lone_door = GoalContext {
            position: (10.5, 1.0, 4.5),
            world: Some(&world),
            ..GoalContext::default()
        };
        no_break.idle_tick(&lone_door);
        // The open door at (8,1,0) is still in range and wins.
        assert_eq!(no_break.dest, Some((8, 1, 0)));
    }

    #[test]
    fn ranged_bow_attack_goal_releases_tipped_arrow_after_draw() {
        let mut goal = RangedBowAttackGoal::new(4, 1.74, Some("minecraft:slowness"), 7);
        goal.hard = RuleProbe(|| false);
        let ctx = GoalContext {
            position: (0.0, 1.0, 0.0),
            target: Some(target(9, 10.0, 1.0, 0.0)),
            ..GoalContext::default()
        };
        goal.start(&ctx);
        let mut release = None;
        for tick in 1..=40 {
            let out = goal.tick(&ctx).unwrap();
            if out.ranged_attack.is_some() {
                release = Some((tick, out));
                break;
            }
        }
        let (tick, out) = release.expect("bow never released");
        assert_eq!(tick, 20);
        assert_eq!(out.attack_target, Some(9));
        let RangedProjectileAction::Arrow {
            tipped, velocity, ..
        } = out.ranged_attack.unwrap()
        else {
            panic!("expected an arrow launch");
        };
        assert_eq!(tipped, Some("minecraft:slowness"));
        let speed = (velocity.0 * velocity.0 + velocity.1 * velocity.1 + velocity.2 * velocity.2)
            .sqrt();
        assert!((speed - 1.6).abs() < 1.0e-9, "arrow speed {speed}");
        // The 40-tick normal-difficulty attack interval then a full 20-tick
        // redraw: the second arrow looses exactly 59 ticks after the first.
        let mut second = None;
        for tick in (tick + 1)..=120 {
            if goal.tick(&ctx).unwrap().ranged_attack.is_some() {
                second = Some(tick);
                break;
            }
        }
        assert_eq!(second, Some(79));
        // Without a target the goal cannot run.
        let no_target = GoalContext::default();
        assert!(!goal.can_use(&no_target));
    }


    #[test]
    fn alpha5_open_door_goal_and_door_capable_navigation_reach_through_wooden_door() {
        let mut world = GridWorld::default();
        world.set(1, 1, 0, BlockPathType::DoorWoodClosed);
        world.set(1, 2, 0, BlockPathType::DoorWoodClosed);
        let ctx = GoalContext {
            position: (0.5, 1.0, 0.5),
            world: Some(&world),
            ..GoalContext::default()
        };
        let mut goal = OpenDoorGoal::new(1);
        assert!(goal.can_use(&ctx));
        goal.start(&ctx);
        let out = goal.tick(&ctx).expect("door goal output");
        assert_eq!(out.open_door, Some((1, 1, 0)));
        assert_eq!(out.move_target, Some((1.5, 1.0, 0.5)));

        // Door interaction changes the navigation contract only for mobs that
        // can operate the door: ordinary walkers still see it as blocked.
        let evaluator = WalkNodeEvaluator {
            can_pass_closed_wooden_doors: true,
            ..WalkNodeEvaluator::default()
        };
        assert!(evaluator.standable(&world, (1, 1, 0)));
        assert!(!WalkNodeEvaluator::default().standable(&world, (1, 1, 0)));
    }

    #[test]
    fn alpha5_creeper_fuse_flint_charge_cancel_and_charged_head_rules() {
        let mut fuse = AiCreeperFuse::new(false);
        for _ in 0..29 {
            assert_eq!(fuse.tick(true), None);
        }
        assert_eq!(fuse.tick(true), Some(3.0));

        let mut cancel = AiCreeperFuse::new(false);
        for _ in 0..10 {
            cancel.tick(true);
        }
        for _ in 0..30 {
            assert_eq!(cancel.tick(false), None);
        }
        assert_eq!(cancel.fuse_ticks, 0, "leaving the seven-block goal radius unwinds the fuse");

        let mut charged = AiCreeperFuse::new(true);
        charged.ignite_flint_and_steel();
        for _ in 0..10 {
            charged.tick(false);
        }
        assert_eq!(charged.fuse_ticks, 10, "flint ignition cannot be cancelled by losing the target");
        for _ in 10..29 {
            assert_eq!(charged.tick(false), None);
        }
        assert_eq!(charged.tick(false), Some(6.0));
        assert_eq!(
            ai_charged_creeper_head_drop("minecraft:wither_skeleton", true),
            Some("minecraft:wither_skeleton_skull")
        );
        assert_eq!(ai_charged_creeper_head_drop("minecraft:piglin", true), Some("minecraft:piglin_head"));
        assert_eq!(ai_charged_creeper_head_drop("minecraft:zombie", false), None);
        assert_eq!(ai_charged_creeper_head_drop("minecraft:cow", true), None);
    }

    #[test]
    fn alpha5_ranged_crossbow_accuracy_charge_and_projectile_helpers() {
        assert_eq!(ai_projectile_inaccuracy(1), 10);
        assert_eq!(ai_projectile_inaccuracy(2), 6);
        assert_eq!(ai_projectile_inaccuracy(3), 2);
        let mut crossbow = AiCrossbowAttack::new();
        for _ in 0..24 {
            assert!(!crossbow.tick(true));
        }
        assert!(crossbow.tick(true));
        for tick in 1..25 {
            assert!(!crossbow.tick(true), "redraw tick {tick}");
        }
        assert!(crossbow.tick(true), "a visible target produces the next bolt after another 25-tick charge");
        let mut reset = AiCrossbowAttack::new();
        for _ in 0..10 {
            reset.tick(true);
        }
        assert!(!reset.tick(false));
        for _ in 0..24 {
            assert!(!reset.tick(true), "losing sight resets the draw");
        }
        assert!(reset.tick(true), "the reset draw needs the full 25 ticks again");
    }

    #[test]
    fn alpha5_nether_and_overworld_ecology_rules_are_deterministic() {
        assert_eq!(ai_slime_split_children(4, 0), Some((2, 2)));
        assert_eq!(ai_slime_split_children(4, 2), Some((2, 4)));
        assert_eq!(ai_slime_split_children(2, 1), Some((1, 3)));
        assert_eq!(ai_slime_split_children(1, 2), None);

        assert!(ai_enderman_can_pick_block(true, true));
        assert!(!ai_enderman_can_pick_block(true, false));
        assert!(!ai_enderman_can_pick_block(false, true));
        assert!(ai_enderman_can_place_block(true, true, true, true));
        assert!(!ai_enderman_can_place_block(true, true, false, true));
        assert!(!ai_enderman_can_place_block(true, true, true, false));

        assert!(ai_phantom_can_target_insomniac(72_000));
        assert!(!ai_phantom_can_target_insomniac(71_999));
        assert_eq!(ai_wolf_variant_for_biome("minecraft:old_growth_pine_taiga"), "minecraft:black");
        assert_eq!(ai_wolf_variant_for_biome("minecraft:sparse_jungle"), "minecraft:rusty");
        assert_eq!(ai_wolf_variant_for_biome("minecraft:savanna_plateau"), "minecraft:spotted");
        assert_eq!(ai_wolf_variant_for_biome("minecraft:wooded_badlands"), "minecraft:striped");
        assert_eq!(ai_wolf_variant_for_biome("minecraft:grove"), "minecraft:snowy");
        assert_eq!(ai_wolf_variant_for_biome("minecraft:plains"), "minecraft:pale");
        assert!(ai_wolf_pack_member_joins_anger(32.0_f64.powi(2), false, false));
        assert!(!ai_wolf_pack_member_joins_anger(32.1_f64.powi(2), false, false));
        assert!(!ai_wolf_pack_member_joins_anger(1.0, true, false));
        assert!(ai_wolf_pack_member_joins_anger(1.0, true, true));
        assert!(ai_wolf_armor_absorbs(false, true));
        assert!(!ai_wolf_armor_absorbs(true, true));
    }

    #[test]
    fn alpha5_passive_ecology_allay_sniffer_camel_bee_frog_and_armadillo_rules() {
        assert_eq!(ai_bee_honey_level_on_exit(2, true, 2_400, 1), 3);
        assert_eq!(ai_bee_honey_level_on_exit(5, true, 2_400, 1), 5);
        assert_eq!(ai_bee_honey_level_on_exit(2, true, 2_399, 0), 2);
        assert_eq!(ai_bee_honey_level_on_exit(2, false, 2_400, 0), 2);
        assert_eq!(ai_bee_colony_anger_ticks(true, false, true, 0), 400);
        assert_eq!(ai_bee_colony_anger_ticks(false, true, false, 0), 400);
        assert_eq!(ai_bee_colony_anger_ticks(false, true, true, 380), 0);
        assert_eq!(ai_bee_colony_anger_ticks(false, true, false, 380), 780);
        assert_eq!(ai_bee_honey_level_on_exit(3, true, 2_400, 0), 5);

        assert!(ai_armadillo_should_roll_up(true, None, None, false, false));
        assert!(ai_armadillo_should_roll_up(false, Some(49.0), None, false, false));
        assert!(!ai_armadillo_should_roll_up(false, Some(49.1), None, false, false));
        assert!(ai_armadillo_should_roll_up(false, None, Some(49.0), true, false));
        assert!(!ai_armadillo_should_roll_up(false, None, Some(49.0), false, false));
        assert!(!ai_armadillo_should_roll_up(true, Some(1.0), Some(1.0), true, true));
        assert_eq!(ai_armadillo_brush_drop(true), Some("minecraft:armadillo_scute"));
        assert_eq!(ai_armadillo_brush_drop(false), None);

        assert!(ai_frog_tongue_prey("minecraft:magma_cube", 1));
        assert!(ai_frog_tongue_prey("minecraft:slime", 1));
        assert!(!ai_frog_tongue_prey("minecraft:magma_cube", 2));
        assert_eq!(ai_froglight_for_variant("warm"), "minecraft:pearlescent_froglight");
        assert_eq!(ai_froglight_for_variant("cold"), "minecraft:verdant_froglight");
        assert_eq!(ai_froglight_for_variant("temperate"), "minecraft:ochre_froglight");

        assert!(ai_allay_note_block_target(100, 699));
        assert!(!ai_allay_note_block_target(100, 700));
        assert!(ai_allay_can_collect_item(true, false, true, 32.0_f64.powi(2)));
        assert!(!ai_allay_can_collect_item(true, false, true, 32.1_f64.powi(2)));
        assert!(!ai_allay_can_collect_item(false, false, true, 1.0));
        assert_eq!(ai_sniffer_dug_seed(0), "minecraft:torchflower_seeds");
        assert_eq!(ai_sniffer_dug_seed(1), "minecraft:pitcher_pod");

        assert_eq!(AiCamelDash::MAX_PASSENGERS, 2);
        let mut camel = AiCamelDash { last_dash_tick: None };
        let (x, z) = camel.try_dash(1_000, false, 0.0).expect("first dash");
        assert!((x - 0.0).abs() < 1e-9 && (z - 1.15).abs() < 1e-9);
        assert_eq!(camel.try_dash(1_054, false, 0.0), None);
        assert!(camel.try_dash(1_055, false, 0.0).is_some());
        let mut sitting = AiCamelDash { last_dash_tick: None };
        assert_eq!(sitting.try_dash(2_000, true, 0.0), None);
    }

    #[test]
    fn alpha5_climb_and_swim_navigation_vectors_are_bounded() {
        assert_eq!(ai_climb_velocity((1.0, -1.0), true), (0.2, 0.2, -0.2));
        assert_eq!(ai_climb_velocity((1.0, -1.0), false), (0.2, 0.0, -0.2));
        let (x, y, z) = ai_swim_velocity((0.0, 0.0, 0.0), (3.0, 0.0, 4.0), 0.5);
        assert!((x - 0.3).abs() < 1e-9);
        assert_eq!(y, 0.0);
        assert!((z - 0.4).abs() < 1e-9);
        assert_eq!(ai_swim_velocity((1.0, 1.0, 1.0), (1.0, 1.0, 1.0), 0.5), (0.0, 0.0, 0.0));
    }

}
