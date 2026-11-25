// SPDX-License-Identifier: LGPL-3.0-or-later

//! Canonical world-view contract shared by static dimensions and moving
//! sub-levels (Create contraptions, Aeronautics-style 6-DOF airships),
//! plus the two pieces of Vanilla 1.21.1 machinery every consumer needs:
//! quarter-turn `BlockState` property rotation matching `Rotation` and a
//! strictly ordered scheduled-tick queue matching `LevelTicks`.
//!
//! * [`WorldView`] — the one read/write/transform surface a subsystem
//!   programs against whether the target is a stationary chunk view
//!   ([`SubLevelTransform::IDENTITY`]) or a rigid sub-level.
//! * [`QuarterTurnY`] / [`rotate_block_properties`] — a generic,
//!   Vanilla-1.21.1-accurate 90°-step rotator for *every* directional
//!   blockstate property (`facing`, `axis`, `rotation`, `orientation`,
//!   rail `shape`, and the cardinal connection keys `north`/`east`/
//!   `south`/`west` used by fences, walls, pipes, redstone and belts).
//! * [`VanillaScheduledTick`] / [`VanillaScheduledTickQueue`] — a queue
//!   drained in exact `(trigger_tick, priority, sub_tick_order)` order,
//!   the same total order Vanilla's `LevelTicks` imposes.
//!
//! Turn convention: `quarter_turns` counts the same direction the rigid
//! pose yaw and the sub-world `(x, z) -> (z, -x)` coordinate map do —
//! `+1` is the right-handed rotation about `+Y`, which is exactly vanilla
//! `Rotation.COUNTERCLOCKWISE_90` for positions **and** directions
//! (`north -> west -> south -> east -> north`). Passing `3` is equivalent
//! to vanilla `CLOCKWISE_90`. Keeping a single convention lets callers
//! rotate a disassembled contraption's coordinates *and* its blockstates
//! with the same `rot` value and no sign flips.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

// ---------------------------------------------------------------------------
// SubLevelTransform / WorldView
// ---------------------------------------------------------------------------

/// Rigid local→world transform of a [`WorldView`]: a row-major rotation
/// matrix `R`, the world `translation` of `pivot`, and the local `pivot`
/// the translation refers to. World-space mapping is
/// `w = translation + R · (p − pivot)`; because `R` is orthonormal the
/// inverse is the transpose. [`SubLevelTransform::IDENTITY`] is the static
/// world: no rotation, no offset, pivot at the local origin.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SubLevelTransform {
    /// Row-major rotation; `world_delta[i] = Σ_j rotation[i][j] · local_delta[j]`.
    pub rotation: [[f64; 3]; 3],
    /// World-space position of `pivot`.
    pub translation: [f64; 3],
    /// The local point `translation` refers to (e.g. a rigid body's center
    /// of mass). `[0, 0, 0]` for origin-anchored sub-levels.
    pub pivot: [f64; 3],
}

impl SubLevelTransform {
    /// Identity transform: `local_to_world` and `world_to_local` are the
    /// identity function.
    pub const IDENTITY: Self = Self {
        rotation: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        translation: [0.0; 3],
        pivot: [0.0; 3],
    };

    /// `true` when this transform is exactly [`SubLevelTransform::IDENTITY`].
    pub fn is_identity(&self) -> bool {
        self == &Self::IDENTITY
    }

    /// `true` when every component is finite — a transform carrying NaN or
    /// infinity must never reach coordinate math.
    pub fn is_finite(&self) -> bool {
        self.rotation
            .iter()
            .flatten()
            .chain(&self.translation)
            .chain(&self.pivot)
            .all(|v| v.is_finite())
    }

    /// `translation + R · (p − pivot)`. `None` when the transform or the
    /// point is non-finite (fail closed, never propagate NaN).
    pub fn local_to_world(&self, p: [f64; 3]) -> Option<[f64; 3]> {
        if !self.is_finite() || !p.iter().all(|v| v.is_finite()) {
            return None;
        }
        let rel = [
            p[0] - self.pivot[0],
            p[1] - self.pivot[1],
            p[2] - self.pivot[2],
        ];
        Some(std::array::from_fn(|i| {
            self.translation[i]
                + self.rotation[i][0] * rel[0]
                + self.rotation[i][1] * rel[1]
                + self.rotation[i][2] * rel[2]
        }))
    }

    /// `pivot + Rᵀ · (w − translation)` — the exact inverse of
    /// [`Self::local_to_world`] for orthonormal `R`. `None` when the
    /// transform or the point is non-finite.
    pub fn world_to_local(&self, w: [f64; 3]) -> Option<[f64; 3]> {
        if !self.is_finite() || !w.iter().all(|v| v.is_finite()) {
            return None;
        }
        let rel = [
            w[0] - self.translation[0],
            w[1] - self.translation[1],
            w[2] - self.translation[2],
        ];
        Some(std::array::from_fn(|i| {
            self.pivot[i]
                + self.rotation[0][i] * rel[0]
                + self.rotation[1][i] * rel[1]
                + self.rotation[2][i] * rel[2]
        }))
    }
}

/// The shared block read/write/transform contract: a static dimension
/// returns [`SubLevelTransform::IDENTITY`]; a rigid sub-level (contraption,
/// airship) returns its live rigid transform so callers can convert local
/// block cells to world coordinates — or invert world queries back to
/// local space — without knowing which kind of view they hold.
///
/// Coordinates are *local* block-cell coordinates: for a static world that
/// is the world cell itself; for a sub-level it is the sub-level-local
/// cell (apply [`SubLevelTransform`] to reach world space).
pub trait WorldView {
    /// The state id occupying local `pos`; `None` for air or cells outside
    /// the view.
    fn block_state_at(&self, pos: [i32; 3]) -> Option<u32>;

    /// Write `state` at local `pos`. Returns `false` when the write was
    /// refused (out of bounds, immutable view).
    fn set_block_state(&mut self, pos: [i32; 3], state: u32) -> bool;

    /// The rigid local→world transform of this view —
    /// [`SubLevelTransform::IDENTITY`] for a static world.
    fn world_transform(&self) -> SubLevelTransform;
}

// ---------------------------------------------------------------------------
// QuarterTurnY + rotate_block_properties
// ---------------------------------------------------------------------------

/// One 90°-step rotation about the `+Y` axis in the sub-world yaw sense
/// (right-handed; the same convention as `Pose` yaw and the `(x, z) ->
/// (z, -x)` disassembly map). `CounterClockwise90` is literally vanilla
/// `Rotation.COUNTERCLOCKWISE_90`, `HalfTurn` is `CLOCKWISE_180`, and
/// `Clockwise90` is `CLOCKWISE_90` — the naming records the vanilla
/// equivalence while `turns()` keeps the sub-world sign convention.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuarterTurnY {
    /// No rotation.
    None,
    /// `+1` turn: `(x, z) -> (z, -x)` — vanilla `COUNTERCLOCKWISE_90`.
    CounterClockwise90,
    /// `+2` turns: `(x, z) -> (-x, -z)` — vanilla `CLOCKWISE_180`.
    HalfTurn,
    /// `+3` turns: `(x, z) -> (-z, x)` — vanilla `CLOCKWISE_90`.
    Clockwise90,
}

impl QuarterTurnY {
    /// Reduce an arbitrary turn count into `0..=3` (negative turns are the
    /// opposite direction, wrapped by Euclidean remainder).
    pub fn from_turns(turns: i32) -> Self {
        match turns.rem_euclid(4) {
            1 => Self::CounterClockwise90,
            2 => Self::HalfTurn,
            3 => Self::Clockwise90,
            _ => Self::None,
        }
    }

    /// The turn count `0..=3` this rotation encodes.
    pub fn turns(self) -> i32 {
        match self {
            Self::None => 0,
            Self::CounterClockwise90 => 1,
            Self::HalfTurn => 2,
            Self::Clockwise90 => 3,
        }
    }

    /// Rotate `(x, z)` by this turn — the same map
    /// `rotate_block_properties` applies to direction *values*.
    pub fn rotate_xz(self, x: i32, z: i32) -> (i32, i32) {
        match self {
            Self::None => (x, z),
            Self::CounterClockwise90 => (z, -x),
            Self::HalfTurn => (-x, -z),
            Self::Clockwise90 => (-z, x),
        }
    }
}

/// Cardinal direction names in the order `+1` turn cycles them:
/// `north -> west -> south -> east -> north`.
const CARDINAL_ORDER: [&str; 4] = ["north", "west", "south", "east"];

/// Rotate a cardinal direction name by `turns` (`+1` = the yaw/CCW sense:
/// `north -> west`). Non-cardinal names (`up`, `down`, or anything else)
/// pass through unchanged so `facing=up` survives a yaw rotation.
fn rotate_direction_name(name: &str, turns: i32) -> &str {
    match CARDINAL_ORDER.iter().position(|&d| d == name) {
        Some(i) => CARDINAL_ORDER[(i + turns as usize) % 4],
        None => name,
    }
}

/// `true` for the four cardinal direction names.
fn is_cardinal(name: &str) -> bool {
    CARDINAL_ORDER.contains(&name)
}

/// One `+1`-turn map over the ten vanilla `RailShape` names: a straight
/// `north_south` becomes `east_west`, every `ascending_*` rotates its
/// direction, and every curve rotates both ends (`south_east` ->
/// `north_east`). Applying it `turns` times gives the 0/180/270 cases.
fn rotate_rail_shape_once(shape: &str) -> &str {
    match shape {
        "north_south" => "east_west",
        "east_west" => "north_south",
        "ascending_east" => "ascending_north",
        "ascending_north" => "ascending_west",
        "ascending_west" => "ascending_south",
        "ascending_south" => "ascending_east",
        "south_east" => "north_east",
        "north_east" => "north_west",
        "north_west" => "south_west",
        "south_west" => "south_east",
        other => other,
    }
}

/// `key` is one of the cardinal connection property names (`north`,
/// `east`, `south`, `west`) used by fences, walls, pipes, redstone wire
/// and belts. Rotate the key the same `turns` as every other direction.
fn rotate_cardinal_key(key: &str, turns: i32) -> &str {
    rotate_direction_name(key, turns)
}

/// Rotate `orientation` values (`up_north`, `down_east`, `north_up`,
/// `east_up`, …): each of the twelve vanilla `Orientation` names carries
/// exactly one horizontal component; that component turns, `up`/`down`
/// parts stay. Unrecognized composites pass through.
fn rotate_orientation_value(value: &str, turns: i32) -> String {
    let Some((a, b)) = value.split_once('_') else {
        return value.to_string();
    };
    let a = rotate_direction_name(a, turns);
    let b = rotate_direction_name(b, turns);
    format!("{a}_{b}")
}

/// Rotate every directional blockstate property in `props` by
/// `quarter_turns` (in the [`QuarterTurnY`] sense, `rem_euclid(4)`).
///
/// Covers the complete Vanilla 1.21.1 directional surface:
/// - any *value* that is a cardinal name (`facing`, `horizontal_facing`,
///   `direction`, …) rotates — `up`/`down` and non-direction values pass
///   through untouched;
/// - `axis`: `x` <-> `z` on odd turns, `y` preserved;
/// - `rotation` (`0..=15`, signs/banners): `(r - 4·turns) mod 16`, matching
///   the direction map exactly (a `south` sign `r=0` lands on `r=12` =
///   `east` after one turn, exactly as `facing=south -> east`);
/// - `orientation` (jigsaw/crafter): the horizontal component rotates;
/// - `shape` for rails: the full ten-name `RailShape` table;
/// - cardinal *keys* (`north`, `east`, `south`, `west`) rotate cyclically
///   for fences, walls, pipes, redstone and belts — their values move to
///   the rotated key unchanged.
///
/// Property order is preserved; unknown keys and values pass through
/// byte-for-byte so the function is safe on modded blockstates.
pub fn rotate_block_properties(
    props: &[(&str, &str)],
    quarter_turns: i32,
) -> Vec<(String, String)> {
    let turns = quarter_turns.rem_euclid(4);
    if turns == 0 {
        return props
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
    }
    // Cardinal connection keys carry their values to the rotated key:
    // new[k] = old[preimage(k)], and preimage under +turns is a -turns
    // rotation of the key itself.
    let cardinal_value_at = |key: &str| -> Option<&str> {
        let pre = rotate_cardinal_key(key, 4 - turns);
        props.iter().find(|(k, _)| *k == pre).map(|(_, v)| *v)
    };
    props
        .iter()
        .map(|&(key, value)| {
            if is_cardinal(key) {
                let value = cardinal_value_at(key).unwrap_or(value);
                return (key.to_string(), value.to_string());
            }
            let value = match key {
                "axis" => match (value, turns % 2) {
                    ("x", 1) => "z",
                    ("z", 1) => "x",
                    (other, _) => other,
                }
                .to_string(),
                "rotation" => match value.parse::<i64>() {
                    Ok(r) if (0..16).contains(&r) => {
                        (r - 4 * i64::from(turns)).rem_euclid(16).to_string()
                    }
                    _ => value.to_string(),
                },
                "orientation" => rotate_orientation_value(value, turns),
                "shape" => {
                    let mut shape = value;
                    for _ in 0..turns {
                        shape = rotate_rail_shape_once(shape);
                    }
                    shape.to_string()
                }
                _ => {
                    if is_cardinal(value) {
                        rotate_direction_name(value, turns).to_string()
                    } else {
                        value.to_string()
                    }
                }
            };
            (key.to_string(), value)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// VanillaScheduledTick / VanillaScheduledTickQueue
// ---------------------------------------------------------------------------

/// One scheduled update in exact Vanilla 1.21.1 `LevelTicks` form: a
/// target position, an opaque payload, and the ordering triple
/// `(trigger_tick, priority, sub_tick_order)`. `priority` is vanilla's
/// `TickPriority` value (`-3` extremely high .. `3` extremely low — lower
/// runs first); `sub_tick_order` is the monotonic insertion counter that
/// breaks `(trigger, priority)` ties strictly by schedule order.
#[derive(Clone, Debug, PartialEq)]
pub struct VanillaScheduledTick<T> {
    /// Target block-cell position (local to the owning world view).
    pub pos: [i32; 3],
    /// Opaque scheduled payload (block/fluid id, closure token, …).
    pub payload: T,
    /// Game tick this update fires on.
    pub trigger_tick: u64,
    /// Vanilla `TickPriority` value; smaller runs first.
    pub priority: i8,
    /// Monotonic schedule counter; breaks `(trigger, priority)` ties.
    pub sub_tick_order: u64,
}

impl<T> VanillaScheduledTick<T> {
    /// The `(trigger_tick, priority, sub_tick_order)` ordering key — the
    /// exact total order `LevelTicks` drains in.
    pub fn order_key(&self) -> (u64, i8, u64) {
        (self.trigger_tick, self.priority, self.sub_tick_order)
    }
}

/// Wrapper giving the heap the exact ordering key while `T` stays
/// unconstrained.
struct OrderedTick<T>(VanillaScheduledTick<T>);

impl<T> PartialEq for OrderedTick<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0.order_key() == other.0.order_key()
    }
}
impl<T> Eq for OrderedTick<T> {}
impl<T> PartialOrd for OrderedTick<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<T> Ord for OrderedTick<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.order_key().cmp(&other.0.order_key())
    }
}

/// A scheduled-tick queue that drains in strict Vanilla 1.21.1
/// `LevelTicks` order: ascending `trigger_tick`, then `priority`
/// (`-3` extremely high runs first), then `sub_tick_order` (insertion
/// order). Entries are never reordered by `swap_remove`-style last-write
/// tricks — the heap pops the globally earliest element every time, so an
/// interleaved schedule/drain stream reproduces vanilla's exact sequence.
#[derive(Default)]
pub struct VanillaScheduledTickQueue<T> {
    heap: BinaryHeap<Reverse<OrderedTick<T>>>,
    next_sub_order: u64,
}

impl<T> VanillaScheduledTickQueue<T> {
    /// Empty queue; `sub_tick_order` starts at `0` and never repeats for
    /// the life of the queue.
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            next_sub_order: 0,
        }
    }

    /// Schedule `payload` at `pos` for `trigger_tick` under `priority`,
    /// assigning the next monotonic `sub_tick_order`. Returns the assigned
    /// order counter.
    pub fn schedule(
        &mut self,
        pos: [i32; 3],
        payload: T,
        trigger_tick: u64,
        priority: i8,
    ) -> u64 {
        let order = self.next_sub_order;
        self.next_sub_order = self.next_sub_order.wrapping_add(1);
        self.heap.push(Reverse(OrderedTick(VanillaScheduledTick {
            pos,
            payload,
            trigger_tick,
            priority,
            sub_tick_order: order,
        })));
        order
    }

    /// Number of queued ticks.
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// `true` when nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// The `trigger_tick` of the next tick to fire, without draining.
    pub fn next_due_tick(&self) -> Option<u64> {
        self.heap.peek().map(|Reverse(t)| t.0.trigger_tick)
    }

    /// The earliest queued tick's full ordering key, without draining.
    pub fn peek_order(&self) -> Option<(u64, i8, u64)> {
        self.heap.peek().map(|Reverse(t)| t.0.order_key())
    }

    /// Remove every tick with `trigger_tick <= now` into `out` in strict
    /// `(trigger_tick, priority, sub_tick_order)` order. Already-returned
    /// ticks never reappear; entries due later stay queued.
    pub fn drain_due(&mut self, now: u64, out: &mut Vec<VanillaScheduledTick<T>>) {
        while let Some(Reverse(tick)) = self.heap.peek() {
            if tick.0.trigger_tick > now {
                break;
            }
            // `pop` yields the global minimum — the heap never needs an
            // unordered removal, so drain order is total and exact.
            let Reverse(tick) = self.heap.pop().expect("peeked element");
            out.push(tick.0);
        }
    }

    /// `true` when a tick for `pos` with an equal `payload` is already
    /// queued — the `LevelChunkTicks` duplicate-schedule guard.
    pub fn already_scheduled(&self, pos: [i32; 3], payload: &T) -> bool
    where
        T: PartialEq,
    {
        self.heap
            .iter()
            .any(|Reverse(t)| t.0.pos == pos && &t.0.payload == payload)
    }

    /// Schedule like [`Self::schedule`] but refuse duplicates on
    /// `(pos, payload)` — returns `None` when one is already queued.
    pub fn schedule_unique(
        &mut self,
        pos: [i32; 3],
        payload: T,
        trigger_tick: u64,
        priority: i8,
    ) -> Option<u64>
    where
        T: PartialEq,
    {
        if self.already_scheduled(pos, &payload) {
            return None;
        }
        Some(self.schedule(pos, payload, trigger_tick, priority))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transform_identity_and_inverse_are_exact() {
        let id = SubLevelTransform::IDENTITY;
        assert!(id.is_identity());
        assert_eq!(id.local_to_world([1.5, -2.0, 3.25]), Some([1.5, -2.0, 3.25]));
        let t = SubLevelTransform {
            rotation: [[0.0, 0.0, 1.0], [0.0, 1.0, 0.0], [-1.0, 0.0, 0.0]],
            translation: [10.0, 64.0, -4.0],
            pivot: [0.5, 0.5, 0.5],
        };
        assert!(!t.is_identity());
        for p in [[0.0, 0.0, 0.0], [3.5, -1.25, 8.0], [-7.0, 2.0, 0.5]] {
            let w = t.local_to_world(p).unwrap();
            let back = t.world_to_local(w).unwrap();
            for i in 0..3 {
                assert!((back[i] - p[i]).abs() < 1e-12, "round-trip drift {back:?}");
            }
        }
        assert_eq!(t.local_to_world(t.pivot), Some(t.translation));
    }

    #[test]
    fn facing_axis_rotation_and_orientation_follow_vanilla() {
        // One turn = vanilla COUNTERCLOCKWISE_90: north -> west.
        let props: Vec<(&str, &str)> = vec![("facing", "north")];
        assert_eq!(
            rotate_block_properties(&props, 1),
            vec![("facing".to_string(), "west".to_string())]
        );
        assert_eq!(
            rotate_block_properties(&props, 2),
            vec![("facing".to_string(), "south".to_string())]
        );
        assert_eq!(
            rotate_block_properties(&props, 3),
            vec![("facing".to_string(), "east".to_string())]
        );
        assert_eq!(
            rotate_block_properties(&props, 4),
            vec![("facing".to_string(), "north".to_string())]
        );
        // Full cycle over every cardinal for every turn count.
        for turns in 1..4 {
            let mut seen = std::collections::BTreeSet::new();
            for d in CARDINAL_ORDER {
                let out = rotate_block_properties(&[("facing", d)], turns);
                assert!(seen.insert(out[0].1.clone()), "two directions collided");
            }
            assert_eq!(seen.len(), 4);
        }
        // Vertical facings are preserved by yaw rotations.
        for turns in 1..4 {
            for v in ["up", "down"] {
                let out = rotate_block_properties(&[("facing", v)], turns);
                assert_eq!(out[0].1, v);
            }
        }
        // Axis: x <-> z on odd turns, y always preserved.
        assert_eq!(
            rotate_block_properties(&[("axis", "x")], 1)[0].1,
            "z"
        );
        assert_eq!(
            rotate_block_properties(&[("axis", "x")], 2)[0].1,
            "x"
        );
        assert_eq!(
            rotate_block_properties(&[("axis", "z")], 3)[0].1,
            "x"
        );
        for turns in 1..4 {
            assert_eq!(
                rotate_block_properties(&[("axis", "y")], turns)[0].1,
                "y"
            );
        }
        // Sign/banner rotation segments: (r - 4·turns) mod 16 keeps pace
        // with the direction map (south r=0 -> east r=12 after one turn).
        assert_eq!(
            rotate_block_properties(&[("rotation", "0")], 1)[0].1,
            "12"
        );
        assert_eq!(
            rotate_block_properties(&[("rotation", "3")], 2)[0].1,
            "11"
        );
        assert_eq!(
            rotate_block_properties(&[("rotation", "0")], 4)[0].1,
            "0"
        );
        // Jigsaw/crafter orientation: exactly one horizontal component turns.
        assert_eq!(
            rotate_block_properties(&[("orientation", "north_up")], 1)[0].1,
            "west_up"
        );
        assert_eq!(
            rotate_block_properties(&[("orientation", "up_east")], 1)[0].1,
            "up_north"
        );
        assert_eq!(
            rotate_block_properties(&[("orientation", "down_south")], 3)[0].1,
            "down_west"
        );
    }

    #[test]
    fn rail_shapes_and_cardinal_keys_rotate_exactly() {
        // Straight + ascending rails.
        assert_eq!(
            rotate_block_properties(&[("shape", "north_south")], 1)[0].1,
            "east_west"
        );
        assert_eq!(
            rotate_block_properties(&[("shape", "north_south")], 2)[0].1,
            "north_south"
        );
        assert_eq!(
            rotate_block_properties(&[("shape", "ascending_east")], 1)[0].1,
            "ascending_north"
        );
        assert_eq!(
            rotate_block_properties(&[("shape", "ascending_north")], 2)[0].1,
            "ascending_south"
        );
        // Curved rails: south_east -> north_east -> north_west -> south_west.
        let mut shape = "south_east".to_string();
        for expected in ["north_east", "north_west", "south_west", "south_east"] {
            shape = rotate_block_properties(&[("shape", shape.as_str())], 1)[0]
                .1
                .clone();
            assert_eq!(shape, expected);
        }
        // Stair `shape` values are not rail names: they pass through so a
        // rotated stair keeps `inner_left`/`outer_right` semantics.
        assert_eq!(
            rotate_block_properties(&[("shape", "inner_left")], 1)[0].1,
            "inner_left"
        );
        // Cardinal connection keys (fence/wall/pipe/redstone): the value at
        // `north` moves to `west` after one turn.
        let props = [
            ("north", "true"),
            ("east", "side"),
            ("south", "none"),
            ("west", "low"),
            ("up", "true"),
            ("waterlogged", "false"),
        ];
        let out = rotate_block_properties(&props, 1);
        let get = |k: &str| out.iter().find(|(key, _)| key == k).unwrap().1.as_str();
        assert_eq!(get("west"), "true", "old north arm must face west");
        assert_eq!(get("north"), "side", "old east arm must face north");
        assert_eq!(get("east"), "none", "old south arm must face east");
        assert_eq!(get("south"), "low", "old west arm must face south");
        assert_eq!(get("up"), "true");
        assert_eq!(get("waterlogged"), "false");
        // Unknown keys/values survive byte-for-byte, order preserved.
        let out2 = rotate_block_properties(&props, 2);
        assert_eq!(out2.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
                   ["north", "east", "south", "west", "up", "waterlogged"]);
        assert_eq!(
            rotate_block_properties(&[("custom", "diagonal")], 3)[0].1,
            "diagonal"
        );
    }

    #[test]
    fn scheduled_ticks_drain_in_exact_vanilla_order() {
        let mut q = VanillaScheduledTickQueue::new();
        // Deliberately schedule out of order: later triggers, priorities and
        // sub-orders interleaved.
        q.schedule([1, 2, 3], "late", 50, 0);
        q.schedule([4, 5, 6], "hi-pri", 10, -3);
        q.schedule([7, 8, 9], "first-same", 10, 0);
        q.schedule([0, 0, 0], "second-same", 10, 0);
        q.schedule([9, 9, 9], "mid", 20, -1);

        let mut out = Vec::new();
        q.drain_due(9, &mut out);
        assert!(out.is_empty());
        q.drain_due(10, &mut out);
        let payloads: Vec<&str> = out.iter().map(|t| t.payload).collect();
        assert_eq!(
            payloads,
            vec!["hi-pri", "first-same", "second-same"],
            "priority first, then insertion order — never reordered"
        );
        assert_eq!(q.next_due_tick(), Some(20));
        q.drain_due(50, &mut out);
        let payloads: Vec<&str> = out.iter().map(|t| t.payload).collect();
        assert_eq!(payloads, vec!["hi-pri", "first-same", "second-same", "mid", "late"]);
        assert!(q.is_empty());
        assert_eq!(q.next_due_tick(), None);
    }

    #[test]
    fn scheduled_ticks_sub_order_is_monotonic_and_uniques_dedup() {
        let mut q = VanillaScheduledTickQueue::new();
        let a = q.schedule([0, 0, 0], 7u32, 1, 0);
        let b = q.schedule([1, 0, 0], 7u32, 1, 0);
        assert!(b > a);
        assert_eq!(q.schedule_unique([0, 0, 0], 7u32, 5, 0), None);
        assert!(q.already_scheduled([0, 0, 0], &7u32));
        assert!(!q.already_scheduled([0, 0, 0], &8u32));
        // A drained slot frees the duplicate guard for a fresh schedule.
        let mut out = Vec::new();
        q.drain_due(1, &mut out);
        assert_eq!(out.len(), 2);
        assert!(q.schedule_unique([0, 0, 0], 7u32, 9, 0).is_some());
    }
}
