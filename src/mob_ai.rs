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
        Open | Walkable | WalkableDoor | Trapdoor | DangerPowdersnow | Rail | DoorOpen | Cocoa => {
            0.0
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) trait PathWorld {
    fn path_type(&self, x: i32, y: i32, z: i32) -> BlockPathType;
    fn is_solid(&self, x: i32, y: i32, z: i32) -> bool;
    fn collision_height(&self, x: i32, y: i32, z: i32) -> f64;
    fn min_y(&self) -> i32;
    fn max_y(&self) -> i32;
}

// BlockState classification uses the actual 1.21.1 registry, including the
// open property of doors/gates and the difference between fluid-bearing and
// liquid blocks (waterlogged stairs still have solid collision geometry).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn classify_block_state_path_type(
    reg: &crate::block_registry::BReg,
    state: u32,
) -> BlockPathType {
    use BlockPathType::*;

    if reg.is_air(state) {
        return Open;
    }
    let name = reg.name_of(state).strip_prefix("minecraft:").unwrap_or(reg.name_of(state));
    match name {
        "water" | "bubble_column" => return Water,
        "lava" => return Lava,
        "fire" | "soul_fire" | "campfire" | "soul_campfire" | "magma_block" => {
            return DamageFire;
        }
        "cactus" | "sweet_berry_bush" | "wither_rose" => return DamageOther,
        "powder_snow" => return PowderSnow,
        "honey_block" => return StickyHoney,
        _ => {}
    }
    if name.ends_with("_fence") || name.ends_with("_wall") {
        return Fence;
    }
    if name.ends_with("_fence_gate") {
        return if reg.get_prop(state, "open").as_deref() == Some("true") {
            Open
        } else {
            Fence
        };
    }
    if name.ends_with("_trapdoor") {
        return Trapdoor;
    }
    if name.ends_with("_door") {
        return if reg.get_prop(state, "open").as_deref() == Some("true") {
            DoorOpen
        } else if name == "iron_door" {
            DoorIronClosed
        } else {
            DoorWoodClosed
        };
    }
    if name == "rail" || name.ends_with("_rail") {
        return Rail;
    }
    if name.ends_with("_leaves") {
        return Leaves;
    }
    if reg.is_liquid_block(state) {
        return if reg.fluid_name(state).contains("lava") {
            Lava
        } else {
            Water
        };
    }
    if reg.is_solid(state) || !reg.is_collision_empty(state) {
        Blocked
    } else {
        Open
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct LiveWorldPathAdapter<'a> {
    pub dimension: crate::worldgen::DimId,
    pub reg: &'a crate::block_registry::BReg,
    block_getter: Box<dyn Fn(crate::worldgen::DimId, i32, i32, i32) -> u32 + 'a>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl<'a> LiveWorldPathAdapter<'a> {
    /// Uses the same dimension-aware block lookup as the live mob tick.
    pub fn new(dimension: crate::worldgen::DimId, reg: &'a crate::block_registry::BReg) -> Self {
        Self::with_block_getter(dimension, reg, block_at_dim)
    }

    /// Allows tests and callers to provide a snapshot or synthetic block map.
    pub fn with_block_getter(
        dimension: crate::worldgen::DimId,
        reg: &'a crate::block_registry::BReg,
        getter: impl Fn(crate::worldgen::DimId, i32, i32, i32) -> u32 + 'a,
    ) -> Self {
        Self {
            dimension,
            reg,
            block_getter: Box::new(getter),
        }
    }

    fn state_at(&self, x: i32, y: i32, z: i32) -> Option<u32> {
        (self.min_y()..=self.max_y())
            .contains(&y)
            .then(|| (self.block_getter)(self.dimension, x, y, z))
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl PathWorld for LiveWorldPathAdapter<'_> {
    fn path_type(&self, x: i32, y: i32, z: i32) -> BlockPathType {
        self.state_at(x, y, z).map_or(BlockPathType::Blocked, |state| {
            classify_block_state_path_type(self.reg, state)
        })
    }

    fn is_solid(&self, x: i32, y: i32, z: i32) -> bool {
        self.state_at(x, y, z)
            .is_none_or(|state| self.reg.is_solid(state))
    }

    fn collision_height(&self, x: i32, y: i32, z: i32) -> f64 {
        self.state_at(x, y, z).map_or(1.0, |state| {
            self.reg
                .collision_boxes(state)
                .fold(0.0_f64, |height, (_, max)| height.max(max[1]))
        })
    }

    fn min_y(&self) -> i32 {
        self.dimension.min_y()
    }

    // PathWorld bounds are inclusive, unlike the dimension's exclusive end.
    fn max_y(&self) -> i32 {
        self.dimension.min_y() + self.dimension.height() - 1
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

    // A clear body requires every occupied block to have nonnegative malus and
    // no collision volume. The node's path type comes from the feet position.
    fn clear_body(&self, world: &dyn PathWorld, pos: (i32, i32, i32)) -> bool {
        let (x, y, z) = pos;
        let Some(top) = y.checked_add(self.height_in_blocks() - 1) else {
            return false;
        };
        if y < world.min_y() || top > world.max_y() {
            return false;
        }
        for block_y in y..=top {
            if default_malus(world.path_type(x, block_y, z)) < 0.0
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

    pub fn next_waypoint(&self) -> Option<(f64, f64, f64)> {
        let node = self.path.as_ref()?.nodes.get(self.next_node)?;
        Some((
            f64::from(node.x) + 0.5,
            f64::from(node.y),
            f64::from(node.z) + 0.5,
        ))
    }

    pub fn advance_if_reached(&mut self, position: (f64, f64, f64), tolerance: f64) {
        if let Some(waypoint) = self.next_waypoint() {
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

/// Selects the goal schedule from the pinned protocol-767 entity registry.
/// Goals are stateful, so a selector is constructed once for each mob.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn build_goal_selector_for_etype(etype: i64) -> GoalSelector {
    let mut selector = GoalSelector::default();
    let Some(spec) = entity_spec_by_id(etype) else {
        return selector;
    };
    if spec.max_health <= 0.0
        || matches!(spec.name, "minecraft:player" | "minecraft:armor_stand")
    {
        return selector;
    }

    selector.add_goal(Box::new(FloatGoal { priority: 0 }));
    match spec.name {
        "minecraft:creeper" => {
            selector.add_goal(Box::new(SwellGoal::new(2)));
            selector.add_goal(Box::new(MeleeAttackGoal::new(3)));
        }
        "minecraft:zombie"
        | "minecraft:husk"
        | "minecraft:drowned"
        | "minecraft:spider"
        | "minecraft:cave_spider"
        | "minecraft:skeleton"
        | "minecraft:stray" => {
            selector.add_goal(Box::new(MeleeAttackGoal::new(2)));
        }
        "minecraft:cow" | "minecraft:pig" | "minecraft:sheep" | "minecraft:chicken" => {
            // Item IDs are the existing 1.21.1 server item mappings.
            let tempting_item = match spec.name {
                "minecraft:pig" => 1097,     // carrot
                "minecraft:chicken" => 853,  // wheat_seeds
                _ => 854,                     // wheat
            };
            selector.add_goal(Box::new(PanicGoal::new(1)));
            selector.add_goal(Box::new(BreedGoal::new(2)));
            selector.add_goal(Box::new(TemptGoal::new(3, tempting_item)));
        }
        _ => {}
    }
    selector.add_goal(Box::new(WaterAvoidingRandomStrollGoal::new(
        6,
        (etype as u64) ^ 0x9e3779b97f4a7c15,
    )));
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
            profile.navigation.navigate_to(&finder, world, start, destination);
            profile.requested_target = Some(destination);
        }
        profile.navigation.advance_if_reached(ctx.position, 0.45);
        waypoint = profile.navigation.next_waypoint();
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

    MobAiStepResult {
        move_target: goal.move_target,
        movement_delta,
        look_yaw,
        look_pitch,
        jump,
        attack_target: goal.attack_target,
        swell: goal.swell,
        breed_target: goal.breed_target,
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

    #[test]
    fn real_registry_block_path_types_and_properties() {
        use BlockPathType::*;
        let reg = crate::block_registry::breg().expect("1.21.1 block registry");
        for (name, expected) in [
            ("stone", Blocked),
            ("air", Open),
            ("cave_air", Open),
            ("short_grass", Open),
            ("water", Water),
            ("bubble_column", Water),
            ("lava", Lava),
            ("oak_fence", Fence),
            ("cobblestone_wall", Fence),
            ("cactus", DamageOther),
            ("sweet_berry_bush", DamageOther),
            ("wither_rose", DamageOther),
            ("fire", DamageFire),
            ("soul_fire", DamageFire),
            ("campfire", DamageFire),
            ("magma_block", DamageFire),
            ("rail", Rail),
            ("powered_rail", Rail),
            ("oak_leaves", Leaves),
            ("powder_snow", PowderSnow),
            ("honey_block", StickyHoney),
            ("oak_trapdoor", Trapdoor),
        ] {
            let state = reg.state_of(name, None);
            assert_eq!(reg.name_of(state), format!("minecraft:{name}"));
            assert_eq!(classify_block_state_path_type(reg, state), expected, "{name}");
        }

        let gate = reg.state_of("oak_fence_gate", None);
        let oak_door = reg.state_of("oak_door", None);
        let iron_door = reg.state_of("iron_door", None);
        assert_eq!(
            classify_block_state_path_type(reg, reg.with_prop(gate, "open", "false")),
            Fence
        );
        assert_eq!(
            classify_block_state_path_type(reg, reg.with_prop(gate, "open", "true")),
            Open
        );
        assert_eq!(
            classify_block_state_path_type(reg, reg.with_prop(oak_door, "open", "false")),
            DoorWoodClosed
        );
        assert_eq!(
            classify_block_state_path_type(reg, reg.with_prop(oak_door, "open", "true")),
            DoorOpen
        );
        assert_eq!(
            classify_block_state_path_type(reg, reg.with_prop(iron_door, "open", "false")),
            DoorIronClosed
        );
        assert_eq!(
            classify_block_state_path_type(reg, reg.with_prop(iron_door, "open", "true")),
            DoorOpen
        );
        let wet_stairs = reg.with_prop(
            reg.state_of("oak_stairs", None),
            "waterlogged",
            "true",
        );
        assert!(reg.is_liquid(wet_stairs));
        assert_eq!(classify_block_state_path_type(reg, wet_stairs), Blocked);
    }

    #[test]
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
        assert_eq!(world.path_type(0, 320, 0), BlockPathType::Blocked);

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
        let mut nav = PathNavigation::default();
        assert!(nav.is_done());
        assert!(nav.navigate_to(
            &PathFinder::default(),
            &GridWorld::default(),
            (0, 1, 0),
            (2, 1, 0)
        ));
        assert_eq!(nav.path().unwrap().nodes.len(), 3);
        assert_eq!(nav.next_waypoint(), Some((1.5, 1.0, 0.5)));
        nav.advance_if_reached((1.5, 1.0, 0.5), 0.1);
        assert_eq!(nav.next_waypoint(), Some((2.5, 1.0, 0.5)));
        nav.advance_if_reached((2.5, 1.0, 0.5), 0.1);
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
}
