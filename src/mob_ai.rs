// R7 GP-1: Minecraft 1.21.1 land-mob navigation and goal scheduling.
// Included in main.rs at crate scope. World lookups are supplied by the caller.

use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum BlockPathType {
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
}

// PathType's default values are independent of a mob's per-type overrides.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn default_malus(path_type: BlockPathType) -> f32 {
    use BlockPathType::*;
    match path_type {
        Blocked | PowderSnow | Fence | Lava | UnpassableRail | DamageOther | DoorWoodClosed
        | DoorIronClosed | Leaves => -1.0,
        Water | WaterBorder | DangerFire | DangerOther | StickyHoney => 8.0,
        DamageFire => 16.0,
        Breach => 4.0,
        Open | Walkable | WalkableDoor | Trapdoor | DangerPowdersnow | Rail | DoorOpen | Cocoa
        | DamageCautious | DangerTrapdoor => 0.0,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
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
}

/// Classifies a single state as in 1.21.1 WalkNodeEvaluator.getPathTypeFromState.
#[cfg_attr(not(test), allow(dead_code))]
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
#[cfg_attr(not(test), allow(dead_code))]
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
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct LiveWorldPathAdapter<'a> {
    reg: &'a crate::block_registry::BReg,
    min_y: i32,
    max_y: i32,
    block_getter: Box<dyn Fn(i32, i32, i32) -> u32 + 'a>,
}

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
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
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PathNode {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub path_type: BlockPathType,
    pub malus: f32,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AiPath {
    pub nodes: Vec<PathNode>,
    pub target: (i32, i32, i32),
    pub reached: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct WalkNodeEvaluator {
    pub mob_height: f64,
    pub max_fall_distance: i32,
}

#[cfg_attr(not(test), allow(dead_code))]
impl Default for WalkNodeEvaluator {
    fn default() -> Self {
        Self {
            mob_height: 1.8,
            max_fall_distance: 3,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
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
            if path_type == BlockPathType::DoorOpen {
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

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug)]
struct SearchRecord {
    g_cost: f64,
    parent: Option<(i32, i32, i32)>,
}

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
fn path_distance(a: (i32, i32, i32), b: (i32, i32, i32)) -> f64 {
    let dx = f64::from(a.0) - f64::from(b.0);
    let dy = f64::from(a.1) - f64::from(b.1);
    let dz = f64::from(a.2) - f64::from(b.2);
    (dx * dx + dy * dy + dz * dz).sqrt()
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct PathFinder {
    pub evaluator: WalkNodeEvaluator,
    pub max_visited_nodes: usize,
}

#[cfg_attr(not(test), allow(dead_code))]
impl Default for PathFinder {
    fn default() -> Self {
        Self {
            evaluator: WalkNodeEvaluator::default(),
            max_visited_nodes: 1024,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, Default)]
pub(crate) struct PathNavigation {
    path: Option<AiPath>,
    next_node: usize,
}

#[cfg_attr(not(test), allow(dead_code))]
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

    pub fn path(&self) -> Option<&AiPath> {
        self.path.as_ref()
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct GoalFlags;

#[cfg_attr(not(test), allow(dead_code))]
impl GoalFlags {
    pub const MOVE: u8 = 1;
    pub const LOOK: u8 = 2;
    pub const JUMP: u8 = 4;
    pub const TARGET: u8 = 8;
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct AiEntityPerception {
    pub entity_id: i32,
    pub position: (f64, f64, f64),
    pub width: f32,
    pub line_of_sight: bool,
    pub held_item: Option<u32>,
    pub in_love: bool,
    pub compatible: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
impl AiEntityPerception {
    pub fn new(entity_id: i32, position: (f64, f64, f64)) -> Self {
        Self {
            entity_id,
            position,
            width: 0.6,
            line_of_sight: true,
            held_item: None,
            in_love: false,
            compatible: false,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
fn entity_distance_sq(a: (f64, f64, f64), b: (f64, f64, f64)) -> f64 {
    let dx = a.0 - b.0;
    let dy = a.1 - b.1;
    let dz = a.2 - b.2;
    dx * dx + dy * dy + dz * dz
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct GoalOutput {
    pub move_target: Option<(f64, f64, f64)>,
    pub look_target: Option<(f64, f64, f64)>,
    pub jump: bool,
    pub attack_target: Option<i32>,
    pub swell: Option<bool>,
    pub breed_target: Option<i32>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl GoalOutput {
    // Outputs are merged in priority order. Only the first owner of each
    // command channel may write it, except that jump is an additive request.
    fn merge_missing(&mut self, next: GoalOutput) {
        self.move_target = self.move_target.or(next.move_target);
        self.look_target = self.look_target.or(next.look_target);
        self.jump |= next.jump;
        self.attack_target = self.attack_target.or(next.attack_target);
        self.swell = self.swell.or(next.swell);
        self.breed_target = self.breed_target.or(next.breed_target);
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) trait Goal {
    fn priority(&self) -> i32;
    fn flags(&self) -> u8;
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool;
    fn can_continue_to_use(&self, ctx: &GoalContext<'_>) -> bool;
    fn is_interruptable(&self) -> bool {
        true
    }
    fn start(&mut self, ctx: &GoalContext<'_>);
    fn tick(&mut self, ctx: &GoalContext<'_>) -> Option<GoalOutput>;
    fn stop(&mut self, ctx: &GoalContext<'_>);
    fn idle_tick(&mut self, _ctx: &GoalContext<'_>) {}
    fn stop_output(&self) -> Option<GoalOutput> {
        None
    }
}

#[cfg_attr(not(test), allow(dead_code))]
struct RunningGoal {
    goal: Box<dyn Goal>,
    running: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Default)]
pub(crate) struct GoalSelector {
    goals: Vec<RunningGoal>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl GoalSelector {
    pub fn add_goal(&mut self, goal: Box<dyn Goal>) {
        self.goals.push(RunningGoal {
            goal,
            running: false,
        });
        // Stable sorting retains registration order for equal priorities.
        self.goals.sort_by_key(|entry| entry.goal.priority());
    }

    pub fn running_count(&self) -> usize {
        self.goals.iter().filter(|entry| entry.running).count()
    }

    pub fn tick(&mut self, ctx: &GoalContext<'_>) -> GoalOutput {
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
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Default)]
pub(crate) struct TargetSelector {
    selector: GoalSelector,
}

#[cfg_attr(not(test), allow(dead_code))]
impl TargetSelector {
    pub fn add_goal(&mut self, goal: Box<dyn Goal>) {
        self.selector.add_goal(goal);
    }

    pub fn tick(&mut self, ctx: &GoalContext<'_>) -> GoalOutput {
        self.selector.tick(ctx)
    }

    pub fn running_count(&self) -> usize {
        self.selector.running_count()
    }

    pub fn stop_all(&mut self, ctx: &GoalContext<'_>) -> GoalOutput {
        self.selector.stop_all(ctx)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct FloatGoal {
    pub priority: i32,
}

#[cfg_attr(not(test), allow(dead_code))]
impl Goal for FloatGoal {
    fn priority(&self) -> i32 {
        self.priority
    }
    fn flags(&self) -> u8 {
        GoalFlags::JUMP
    }
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool {
        ctx.in_water || ctx.in_lava
    }
    fn can_continue_to_use(&self, ctx: &GoalContext<'_>) -> bool {
        self.can_use(ctx)
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

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct SwellGoal {
    pub priority: i32,
    ignited: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
impl SwellGoal {
    pub fn new(priority: i32) -> Self {
        Self {
            priority,
            ignited: false,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl Goal for SwellGoal {
    fn priority(&self) -> i32 {
        self.priority
    }
    fn flags(&self) -> u8 {
        GoalFlags::MOVE
    }
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool {
        ctx.target
            .is_some_and(|target| entity_distance_sq(ctx.position, target.position) < 9.0)
    }
    fn can_continue_to_use(&self, ctx: &GoalContext<'_>) -> bool {
        self.ignited || self.can_use(ctx)
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

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct MeleeAttackGoal {
    pub priority: i32,
    attack_cooldown: u32,
}

#[cfg_attr(not(test), allow(dead_code))]
impl MeleeAttackGoal {
    pub fn new(priority: i32) -> Self {
        Self {
            priority,
            attack_cooldown: 0,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl Goal for MeleeAttackGoal {
    fn priority(&self) -> i32 {
        self.priority
    }
    fn flags(&self) -> u8 {
        GoalFlags::MOVE | GoalFlags::LOOK
    }
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool {
        ctx.target.is_some()
    }
    fn can_continue_to_use(&self, ctx: &GoalContext<'_>) -> bool {
        self.can_use(ctx)
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

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct PanicGoal {
    pub priority: i32,
    destination: Option<(f64, f64, f64)>,
}

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
impl Goal for PanicGoal {
    fn priority(&self) -> i32 {
        self.priority
    }
    fn flags(&self) -> u8 {
        GoalFlags::MOVE
    }
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool {
        ctx.on_fire || ctx.recently_hurt
    }
    fn can_continue_to_use(&self, ctx: &GoalContext<'_>) -> bool {
        self.can_use(ctx)
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

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct TemptGoal {
    pub priority: i32,
    pub tempting_item: u32,
    pub calm_down_on_interrupt: bool,
    calm_down: u32,
}

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
impl Goal for TemptGoal {
    fn priority(&self) -> i32 {
        self.priority
    }
    fn flags(&self) -> u8 {
        GoalFlags::MOVE | GoalFlags::LOOK
    }
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool {
        self.calm_down == 0
            && ctx.nearest_player.is_some_and(|player| {
                player.held_item == Some(self.tempting_item)
                    && entity_distance_sq(ctx.position, player.position) < 100.0
            })
    }
    fn can_continue_to_use(&self, ctx: &GoalContext<'_>) -> bool {
        self.can_use(ctx)
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

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct BreedGoal {
    pub priority: i32,
    nearby_ticks: u32,
    completed: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
impl Goal for BreedGoal {
    fn priority(&self) -> i32 {
        self.priority
    }
    fn flags(&self) -> u8 {
        GoalFlags::MOVE | GoalFlags::LOOK
    }
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool {
        Self::valid_mate(ctx).is_some() && !self.completed
    }
    fn can_continue_to_use(&self, ctx: &GoalContext<'_>) -> bool {
        self.can_use(ctx)
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

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct WaterAvoidingRandomStrollGoal {
    pub priority: i32,
    seed: u64,
    destination: Option<(f64, f64, f64)>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl WaterAvoidingRandomStrollGoal {
    pub fn new(priority: i32, seed: u64) -> Self {
        Self {
            priority,
            seed,
            destination: None,
        }
    }

    fn next_random(&mut self) -> u64 {
        self.seed = self.seed.wrapping_add(0x9e3779b97f4a7c15);
        let mut value = self.seed;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
        value ^ (value >> 31)
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

#[cfg_attr(not(test), allow(dead_code))]
impl Goal for WaterAvoidingRandomStrollGoal {
    fn priority(&self) -> i32 {
        self.priority
    }
    fn flags(&self) -> u8 {
        GoalFlags::MOVE
    }
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

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct LookAtPlayerGoal {
    pub priority: i32,
    pub look_distance: f64,
}

#[cfg_attr(not(test), allow(dead_code))]
impl Goal for LookAtPlayerGoal {
    fn priority(&self) -> i32 {
        self.priority
    }
    fn flags(&self) -> u8 {
        GoalFlags::LOOK
    }
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool {
        ctx.nearest_player.is_some_and(|player| {
            entity_distance_sq(ctx.position, player.position)
                <= self.look_distance * self.look_distance
        })
    }
    fn can_continue_to_use(&self, ctx: &GoalContext<'_>) -> bool {
        self.can_use(ctx)
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

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MobGoalKind {
    Float,
    Swell,
    MeleeAttack,
    WaterAvoidingRandomStroll,
    LookAtPlayer,
    Unavailable(&'static str),
}

/// The equipment and door-breaking state that changes vanilla goal registration.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Default)]
pub(crate) struct MobGoalSettings {
    pub skeleton_has_bow: bool,
    pub zombie_can_break_doors: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct MobGoalPlan {
    /// Ordered by priority, preserving vanilla's registration order for ties.
    pub goals: Vec<(i32, MobGoalKind)>,
    /// Target goals need inputs the current engine does not supply.
    pub missing_targets: &'static [(i32, &'static str)],
}

#[cfg_attr(not(test), allow(dead_code))]
impl MobGoalPlan {
    pub fn for_entity(etype: i64, settings: MobGoalSettings) -> Option<Self> {
        use MobGoalKind::*;
        let entity = entity_spec_by_id(etype)?;
        let (mut goals, missing_targets): (Vec<_>, &[_]) = match entity.name {
            "minecraft:zombie" => (
                vec![
                    (2, Unavailable("ZombieAttackGoal")),
                    (4, Unavailable("ZombieAttackTurtleEggGoal")),
                    (6, Unavailable("MoveThroughVillageGoal")),
                    (7, WaterAvoidingRandomStroll),
                    (8, LookAtPlayer),
                    (8, Unavailable("RandomLookAroundGoal")),
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
            ),
            "minecraft:skeleton" => (
                vec![
                    (2, Unavailable("RestrictSunGoal")),
                    (3, Unavailable("FleeSunGoal")),
                    (3, Unavailable("AvoidEntityGoal<Wolf>")),
                    (
                        4,
                        if settings.skeleton_has_bow {
                            Unavailable("RangedBowAttackGoal")
                        } else {
                            MeleeAttack
                        },
                    ),
                    (5, WaterAvoidingRandomStroll),
                    (6, LookAtPlayer),
                    (6, Unavailable("RandomLookAroundGoal")),
                ],
                &[
                    (1, "HurtByTargetGoal"),
                    (2, "NearestAttackableTargetGoal<Player>"),
                    (3, "NearestAttackableTargetGoal<IronGolem>"),
                    (3, "NearestAttackableTargetGoal<Turtle> (baby on land)"),
                ],
            ),
            "minecraft:creeper" => (
                vec![
                    (1, Float),
                    (2, Swell),
                    (3, Unavailable("AvoidEntityGoal<Ocelot>")),
                    (3, Unavailable("AvoidEntityGoal<Cat>")),
                    (4, MeleeAttack),
                    (5, WaterAvoidingRandomStroll),
                    (6, LookAtPlayer),
                    (6, Unavailable("RandomLookAroundGoal")),
                ],
                &[
                    (1, "NearestAttackableTargetGoal<Player>"),
                    (2, "HurtByTargetGoal"),
                ],
            ),
            "minecraft:spider" => (
                vec![
                    (1, Float),
                    (2, Unavailable("AvoidEntityGoal<Armadillo> (unscared)")),
                    (3, Unavailable("LeapAtTargetGoal")),
                    (4, Unavailable("SpiderAttackGoal")),
                    (5, WaterAvoidingRandomStroll),
                    (6, LookAtPlayer),
                    (6, Unavailable("RandomLookAroundGoal")),
                ],
                &[
                    (1, "HurtByTargetGoal"),
                    (2, "SpiderTargetGoal<Player>"),
                    (3, "SpiderTargetGoal<IronGolem>"),
                ],
            ),
            _ => return None,
        };
        if entity.name == "minecraft:zombie" && settings.zombie_can_break_doors {
            goals.insert(0, (1, Unavailable("BreakDoorGoal")));
        }
        Some(Self {
            goals,
            missing_targets,
        })
    }

    pub fn build_selector(&self, seed: u64) -> GoalSelector {
        let mut selector = GoalSelector::default();
        for &(priority, kind) in &self.goals {
            let goal: Box<dyn Goal> = match kind {
                MobGoalKind::Float => Box::new(FloatGoal { priority }),
                MobGoalKind::Swell => Box::new(SwellGoal::new(priority)),
                MobGoalKind::MeleeAttack => Box::new(MeleeAttackGoal::new(priority)),
                MobGoalKind::WaterAvoidingRandomStroll => {
                    Box::new(WaterAvoidingRandomStrollGoal::new(priority, seed))
                }
                MobGoalKind::LookAtPlayer => Box::new(LookAtPlayerGoal {
                    priority,
                    look_distance: 8.0,
                }),
                MobGoalKind::Unavailable(_) => continue,
            };
            selector.add_goal(goal);
        }
        selector
    }
}

/// Selects the goal schedule from the pinned protocol-767 entity registry.
/// Goals are stateful, so a selector is constructed once for each mob.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn build_goal_selector_for_etype(etype: i64) -> GoalSelector {
    let Some(spec) = entity_spec_by_id(etype) else {
        return GoalSelector::default();
    };
    if spec.max_health <= 0.0 || matches!(spec.name, "minecraft:player" | "minecraft:armor_stand") {
        return GoalSelector::default();
    }

    let seed = (etype as u64) ^ 0x9e3779b97f4a7c15;
    if let Some(plan) = MobGoalPlan::for_entity(etype, MobGoalSettings::default()) {
        let mut selector = plan.build_selector(seed);
        // The tick driver has fluid and target observations for these
        // existing float and melee behaviors.
        if matches!(spec.name, "minecraft:zombie" | "minecraft:skeleton") {
            selector.add_goal(Box::new(FloatGoal { priority: 0 }));
        }
        if let Some((priority, _)) = plan.goals.iter().find(|(_, kind)| {
            matches!(
                kind,
                MobGoalKind::Unavailable("ZombieAttackGoal" | "SpiderAttackGoal")
            )
        }) {
            selector.add_goal(Box::new(MeleeAttackGoal::new(*priority)));
        }
        return selector;
    }

    let mut selector = GoalSelector::default();
    selector.add_goal(Box::new(FloatGoal { priority: 0 }));
    match spec.name {
        "minecraft:husk" | "minecraft:drowned" | "minecraft:cave_spider" | "minecraft:stray" => {
            selector.add_goal(Box::new(MeleeAttackGoal::new(2)));
        }
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
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct MobAiProfile {
    pub etype: i64,
    pub selector: GoalSelector,
    pub navigation: PathNavigation,
    pub movement_speed: f64,
    pub eye_height: f64,
    /// Ticks left in a spider's LeapAtTargetGoal pounce impulse.
    spider_leap: u8,
    /// Cooldown before the next leap may start.
    spider_leap_cooldown: u8,
    /// Skeleton bow draw countdown in ticks; `u32::MAX` = not yet drawn.
    bow_charge: u32,
    requested_target: Option<(i32, i32, i32)>,
}

#[cfg_attr(not(test), allow(dead_code))]
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
            navigation: PathNavigation::default(),
            movement_speed,
            eye_height: spec.map_or(1.53, |spec| f64::from(spec.eye_height)),
            spider_leap: 0,
            spider_leap_cooldown: 0,
            bow_charge: u32::MAX,
            requested_target: None,
        }
    }
}

/// Intent to be applied by the live mob physics and interaction code.
/// Movement is an X/Z displacement per tick; Y movement stays with physics.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct MobAiStepResult {
    pub move_target: Option<(f64, f64, f64)>,
    pub movement_delta: (f64, f64, f64),
    pub look_yaw: Option<f32>,
    pub look_pitch: Option<f32>,
    pub jump: bool,
    pub attack_target: Option<i32>,
    pub swell: Option<bool>,
    pub breed_target: Option<i32>,
    /// `maxEntityCramming` damage evaluated for this tick; the live push
    /// engine applies it through its own hurt path.
    pub cramming_damage: f32,
}

/// One tick of scheduling, path-following and intent generation. The caller
/// supplies observations and a PathWorld, then applies the returned actions.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn tick_mob_ai_step(
    profile: &mut MobAiProfile,
    ctx: &GoalContext<'_>,
) -> MobAiStepResult {
    let goal = profile.selector.tick(ctx);
    let mut waypoint = goal.move_target;
    if let (Some(world), Some(target)) = (ctx.world, goal.move_target) {
        let destination = (
            target.0.floor() as i32,
            target.1.floor() as i32,
            target.2.floor() as i32,
        );
        if profile.requested_target != Some(destination) || profile.navigation.is_done() {
            let start = (
                ctx.position.0.floor() as i32,
                ctx.position.1.floor() as i32,
                ctx.position.2.floor() as i32,
            );
            let finder = PathFinder {
                evaluator: WalkNodeEvaluator {
                    mob_height: ctx.height,
                    ..WalkNodeEvaluator::default()
                },
                ..PathFinder::default()
            };
            profile
                .navigation
                .navigate_to(&finder, world, start, destination);
            profile.requested_target = Some(destination);
        }
        profile
            .navigation
            .advance_if_reached(world, ctx.position, 0.45);
        waypoint = profile.navigation.next_waypoint(world);
    } else {
        profile.navigation.stop();
        profile.requested_target = None;
    }

    let mut movement_delta = (0.0, 0.0, 0.0);
    let mut jump = goal.jump;
    if let Some(point) = waypoint {
        let dx = point.0 - ctx.position.0;
        let dz = point.2 - ctx.position.2;
        let distance = dx.hypot(dz);
        if distance > 0.05 {
            let step = profile.movement_speed.max(0.0).min(distance);
            movement_delta = (dx / distance * step, 0.0, dz / distance * step);
        }
        jump |= point.1 > ctx.position.1 + 0.5;
    }

    let (look_yaw, look_pitch) = if let Some(point) = goal.look_target {
        let dx = point.0 - ctx.position.0;
        let dz = point.2 - ctx.position.2;
        let dy = point.1 - (ctx.position.1 + profile.eye_height);
        let horizontal_distance = dx.hypot(dz);
        if horizontal_distance > 0.0 || dy != 0.0 {
            (
                Some((-dx.atan2(dz)).to_degrees() as f32),
                Some((-dy.atan2(horizontal_distance)).to_degrees() as f32),
            )
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    // Hostile-mob velocity helpers (1.21.1): per-tick impulses that replace
    // the waypoint stride while the matching goal is active. `movement_delta`
    // is applied verbatim by the live mover, so these run every `mob_tick`.
    let mut attack_target = goal.attack_target;
    match profile.etype {
        // minecraft:spider — LeapAtTargetGoal pounces inside melee reach.
        100 => {
            profile.spider_leap_cooldown = profile.spider_leap_cooldown.saturating_sub(1);
            if profile.spider_leap > 0 {
                profile.spider_leap -= 1;
                if let Some(target) = ctx.target {
                    let leap = spider_leap_velocity(
                        [ctx.position.0, ctx.position.1, ctx.position.2],
                        [target.position.0, target.position.1, target.position.2],
                    );
                    movement_delta = (leap[0], 0.0, leap[2]);
                    jump = true;
                }
            } else if profile.spider_leap_cooldown == 0
                && ctx.target.is_some_and(|target| {
                    target.line_of_sight
                        && entity_distance_sq(ctx.position, target.position) <= 16.0
                })
            {
                profile.spider_leap = 3;
                profile.spider_leap_cooldown = 20;
            }
        }
        // minecraft:drowned — DrownedSwimGoal chases through water column.
        27 if ctx.in_water => {
            if let Some(point) = goal.move_target {
                let swim = drowned_swim_chase_velocity(
                    [ctx.position.0, ctx.position.1, ctx.position.2],
                    [point.0, point.1, point.2],
                );
                movement_delta = (swim[0], 0.0, swim[2]);
            }
        }
        // minecraft:skeleton — RangedBowAttackGoal strafes inside the 5..15
        // band and only looses once the bow finishes its draw. The plan marks
        // that goal unavailable, so this arm owns both the draw timer and the
        // release; `u32::MAX` marks "not yet drawn".
        91 => {
            if let Some(target) = ctx.target.filter(|target| target.line_of_sight) {
                let distance = entity_distance_sq(ctx.position, target.position).sqrt();
                if (5.0..=15.0).contains(&distance) {
                    let strafe = skeleton_strafe_velocity(
                        distance,
                        profile.etype % 2 == 0,
                        distance > 10.0,
                    );
                    movement_delta = (strafe[0], 0.0, strafe[2]);
                }
                let interval = || {
                    u32::from(skeleton_bow_charge_interval(
                        DIFFICULTY.load(Ordering::Relaxed) >= 3,
                    ))
                };
                if profile.bow_charge == u32::MAX {
                    profile.bow_charge = interval();
                } else {
                    profile.bow_charge = profile.bow_charge.saturating_sub(1);
                }
                if profile.bow_charge == 0 && distance <= 15.0 {
                    attack_target = Some(target.entity_id);
                    profile.bow_charge = interval();
                }
            } else {
                profile.bow_charge = u32::MAX;
            }
        }
        _ => {}
    }

    // `maxEntityCramming`: the live push engine owns the damage application;
    // the step only reports this tick's evaluation for the caller.
    let mut cramming_damage = 0.0f32;
    if let Some(world) = ctx.world {
        if let Some((pushable, non_passengers, roll_mod_4)) =
            world.cramming_census(ctx.position.0, ctx.position.1, ctx.position.2)
        {
            cramming_damage = eval_entity_cramming_damage(
                max_entity_cramming_limit(),
                pushable,
                non_passengers,
                roll_mod_4,
            );
        }
    }

    MobAiStepResult {
        move_target: goal.move_target,
        movement_delta,
        look_yaw,
        look_pitch,
        jump,
        attack_target,
        swell: goal.swell,
        breed_target: goal.breed_target,
        cramming_damage,
    }
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
    }

    impl GridWorld {
        fn set(&mut self, x: i32, y: i32, z: i32, ty: BlockPathType) {
            self.blocks.insert((x, y, z), ty);
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

    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
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
        let mut digest = 0xcbf29ce484222325_u64;
        for state in 0..registry.state_count() as u32 {
            let path_type = classify_block_state_path_type(registry, state);
            for byte in format!("{path_type:?}\n").bytes() {
                digest = (digest ^ u64::from(byte)).wrapping_mul(0x100000001b3);
            }
        }
        assert_eq!(digest, 0x54a74560ce1a30cc);
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
                    (2, Unavailable("ZombieAttackGoal")),
                    (4, Unavailable("ZombieAttackTurtleEggGoal")),
                    (6, Unavailable("MoveThroughVillageGoal")),
                    (7, WaterAvoidingRandomStroll),
                    (8, LookAtPlayer),
                    (8, Unavailable("RandomLookAroundGoal")),
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
                &[7, 8],
            ),
            (
                "skeleton",
                &[
                    (2, Unavailable("RestrictSunGoal")),
                    (3, Unavailable("FleeSunGoal")),
                    (3, Unavailable("AvoidEntityGoal<Wolf>")),
                    (4, Unavailable("RangedBowAttackGoal")),
                    (5, WaterAvoidingRandomStroll),
                    (6, LookAtPlayer),
                    (6, Unavailable("RandomLookAroundGoal")),
                ],
                &[
                    (1, "HurtByTargetGoal"),
                    (2, "NearestAttackableTargetGoal<Player>"),
                    (3, "NearestAttackableTargetGoal<IronGolem>"),
                    (3, "NearestAttackableTargetGoal<Turtle> (baby on land)"),
                ],
                &[5, 6],
            ),
            (
                "creeper",
                &[
                    (1, Float),
                    (2, Swell),
                    (3, Unavailable("AvoidEntityGoal<Ocelot>")),
                    (3, Unavailable("AvoidEntityGoal<Cat>")),
                    (4, MeleeAttack),
                    (5, WaterAvoidingRandomStroll),
                    (6, LookAtPlayer),
                    (6, Unavailable("RandomLookAroundGoal")),
                ],
                &[
                    (1, "NearestAttackableTargetGoal<Player>"),
                    (2, "HurtByTargetGoal"),
                ],
                &[1, 2, 4, 5, 6],
            ),
            (
                "spider",
                &[
                    (1, Float),
                    (2, Unavailable("AvoidEntityGoal<Armadillo> (unscared)")),
                    (3, Unavailable("LeapAtTargetGoal")),
                    (4, Unavailable("SpiderAttackGoal")),
                    (5, WaterAvoidingRandomStroll),
                    (6, LookAtPlayer),
                    (6, Unavailable("RandomLookAroundGoal")),
                ],
                &[
                    (1, "HurtByTargetGoal"),
                    (2, "SpiderTargetGoal<Player>"),
                    (3, "SpiderTargetGoal<IronGolem>"),
                ],
                &[1, 5, 6],
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
        assert_eq!(unarmed.build_selector(0).goals[0].goal.priority(), 4);
        let breaking = MobGoalPlan::for_entity(
            entity_id("zombie"),
            MobGoalSettings {
                zombie_can_break_doors: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            breaking.goals[0],
            (1, MobGoalKind::Unavailable("BreakDoorGoal"))
        );
        assert!(MobGoalPlan::for_entity(entity_id("cow"), MobGoalSettings::default()).is_none());
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn mob_goal_factories_and_tick_driver() {
        let mut creeper = build_goal_selector_for_etype(23);
        let mut zombie = build_goal_selector_for_etype(124);
        let mut cow = build_goal_selector_for_etype(22);
        assert_eq!(creeper.goals.len(), 5);
        assert_eq!(zombie.goals.len(), 4);
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
        // The first close tick only arms the pounce; the impulse lands on the
        // following ticks through the same live step.
        let armed = tick_mob_ai_step(&mut profile, &ctx);
        assert!(!armed.jump || armed.movement_delta != (0.4, 0.0, 0.0));
        let leap = tick_mob_ai_step(&mut profile, &ctx);
        assert!(leap.jump);
        assert_eq!(leap.movement_delta, (0.4, 0.0, 0.0));
        assert_eq!(tick_mob_ai_step(&mut profile, &ctx).movement_delta, (0.4, 0.0, 0.0));
        assert_eq!(tick_mob_ai_step(&mut profile, &ctx).movement_delta, (0.4, 0.0, 0.0));
        // After the 3-tick impulse and the cooldown the waypoint stride returns.
        for _ in 0..4 {
            tick_mob_ai_step(&mut profile, &ctx);
        }
        let after = tick_mob_ai_step(&mut profile, &ctx);
        assert!(!after.jump || after.movement_delta != (0.4, 0.0, 0.0));
    }

    #[test]
    fn v1211_veh_ai_skeleton_strafe_and_bow_draw_run_through_live_step() {
        // Strafe band: a visible target 12 blocks out strafes forward-left.
        let mut profile = MobAiProfile::new(91);
        let ctx = GoalContext {
            position: (0.0, 1.0, 0.0),
            target: Some(target(9, 12.0, 1.0, 0.0)),
            ..GoalContext::default()
        };
        let step = tick_mob_ai_step(&mut profile, &ctx);
        // skeleton_strafe_velocity(12, clockwise=false, forward=true).
        assert_eq!(step.movement_delta, (-0.25, 0.0, 0.25));

        // Bow draw: melee reach keeps attack_target desired, but the shot is
        // suppressed until skeleton_bow_charge_interval ticks elapse.
        let mut profile = MobAiProfile::new(91);
        let close = GoalContext {
            position: (0.0, 1.0, 0.0),
            target: Some(target(9, 1.5, 1.0, 0.0)),
            ..GoalContext::default()
        };
        let interval = u32::from(skeleton_bow_charge_interval(
            DIFFICULTY.load(Ordering::Relaxed) >= 3,
        ));
        assert_eq!(tick_mob_ai_step(&mut profile, &close).attack_target, None);
        assert_eq!(profile.bow_charge, interval);
        let mut first_shot = None;
        for tick in 1..=80 {
            let step = tick_mob_ai_step(&mut profile, &close);
            if step.attack_target.is_some() {
                first_shot = Some(tick);
                break;
            }
        }
        // The arrow looses exactly one draw-interval after the first sighting.
        assert_eq!(first_shot, Some(interval as usize));
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
}
