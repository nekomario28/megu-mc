// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! Hierarchical hashed timing wheel after Varghese & Lauck.
//!
//! Three wheel levels cover 256 ticks, 16 384 ticks, and 1 048 576 ticks with
//! one-slot granularity at the bottom; longer delays wait in an overflow list
//! until they enter the representable horizon.  Entries are held in a
//! generational slab and every wheel slot is an intrusive safe-index linked
//! list, so cancellation is a true O(1) unlink rather than a slot scan.
//!
//! Time is measured in completed wheel advances.  Following Minecraft's
//! scheduled-tick convention, `schedule_after(delay)` treats both zero and one
//! as “next advance” and otherwise fires exactly `delay` advances later.  Zero
//! is therefore safe rather than re-entrant: it can never fire inside the
//! scheduling call itself.

const L0_BITS: u32 = 8;
const L1_BITS: u32 = 6;
const L2_BITS: u32 = 6;
const L0_SIZE: usize = 1 << L0_BITS;
const L1_SIZE: usize = 1 << L1_BITS;
const L2_SIZE: usize = 1 << L2_BITS;
const L0_MASK: u64 = (L0_SIZE as u64) - 1;
const L1_SHIFT: u32 = L0_BITS;
const L2_SHIFT: u32 = L0_BITS + L1_BITS;
const L1_SPAN: u64 = 1 << L1_SHIFT;
const L2_SPAN: u64 = 1 << L2_SHIFT;
/// Number of bottom-level ticks covered by the three hierarchical wheels.
pub const TIMING_WHEEL_SPAN_TICKS: u64 = 1 << (L0_BITS + L1_BITS + L2_BITS);
const NIL: u32 = u32::MAX;

/// Opaque cancellation handle returned by [`TimingWheel::schedule_after`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TimerHandle(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Location {
    Level0(u8),
    Level1(u8),
    Level2(u8),
    Overflow,
}

#[derive(Debug)]
struct Node<T> {
    payload: Option<T>,
    expiry: u64,
    generation: u32,
    prev: u32,
    next: u32,
    location: Location,
    occupied: bool,
}

/// Hierarchical timing wheel storing arbitrary payloads.
///
/// Scheduling and cancellation are O(1) index/linked-list operations.  One
/// advance is O(1) amortized: normal ticks touch only the expiring bottom
/// slot, while boundary ticks cascade the one higher-level slot whose time
/// range has become due.
#[derive(Debug)]
pub struct TimingWheel<T> {
    current_tick: u64,
    level0: [u32; L0_SIZE],
    level1: [u32; L1_SIZE],
    level2: [u32; L2_SIZE],
    overflow_head: u32,
    next_overflow_scan: Option<u64>,
    nodes: Vec<Node<T>>,
    free: Vec<u32>,
    live: usize,
}

impl<T> TimingWheel<T> {
    /// Create an empty wheel at tick zero.
    pub fn new() -> Self {
        Self {
            current_tick: 0,
            level0: [NIL; L0_SIZE],
            level1: [NIL; L1_SIZE],
            level2: [NIL; L2_SIZE],
            overflow_head: NIL,
            next_overflow_scan: None,
            nodes: Vec::new(),
            free: Vec::new(),
            live: 0,
        }
    }

    /// Current number of completed advances.
    pub fn current_tick(&self) -> u64 {
        self.current_tick
    }

    /// Number of live timers.
    pub fn len(&self) -> usize {
        self.live
    }

    /// Whether no timers are live.
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// Schedule `payload` `max(delay, 1)` advances in the future.
    ///
    /// Both `delay == 0` and `delay == 1` fire on the next
    /// [`advance_into`](Self::advance_into) call, matching vanilla scheduled
    /// ticks.  The returned handle is generation-checked: stale handles can
    /// never cancel a recycled node.
    pub fn schedule_after(&mut self, delay_ticks: u64, payload: T) -> TimerHandle {
        let expiry = self
            .current_tick
            .checked_add(delay_ticks.max(1))
            .expect("timer expiry overflowed u64 tick space");
        let index = self.allocate_node(payload, expiry);
        self.link_node(index);
        self.live += 1;
        encode_handle(index, self.nodes[index as usize].generation)
    }

    /// Cancel a live timer in O(1).  Returns `false` for stale/fired handles.
    pub fn cancel(&mut self, handle: TimerHandle) -> bool {
        let Some(index) = self.valid_node(handle) else {
            return false;
        };
        self.unlink_node(index);
        self.retire_detached_node(index);
        self.live -= 1;
        true
    }

    /// Advance by exactly one tick, appending every payload that expires at
    /// the new current tick to `out`.
    ///
    /// Output order within one tick is wheel-slot order, not scheduling order;
    /// callers that require ordering must carry a sequence key in `T`.
    pub fn advance_into(&mut self, out: &mut Vec<T>) {
        self.current_tick = self
            .current_tick
            .checked_add(1)
            .expect("timing-wheel tick overflowed u64");
        let tick = self.current_tick;

        if self
            .next_overflow_scan
            .is_some_and(|scan_tick| scan_tick <= tick)
        {
            self.scan_overflow();
        }
        if tick.is_multiple_of(L2_SPAN) {
            let slot = ((tick >> L2_SHIFT) as usize) & (L2_SIZE - 1);
            self.cascade_slot(Location::Level2(slot as u8));
        }
        if tick.is_multiple_of(L1_SPAN) {
            let slot = ((tick >> L1_SHIFT) as usize) & (L1_SIZE - 1);
            self.cascade_slot(Location::Level1(slot as u8));
        }

        let slot = (tick & L0_MASK) as usize;
        let mut cursor = self.level0[slot];
        self.level0[slot] = NIL;
        while cursor != NIL {
            let index = cursor;
            cursor = self.nodes[index as usize].next;
            debug_assert!(self.nodes[index as usize].expiry <= tick);
            let payload = self.nodes[index as usize]
                .payload
                .take()
                .expect("occupied timer node always has a payload");
            self.nodes[index as usize].next = NIL;
            self.nodes[index as usize].prev = NIL;
            self.retire_detached_node(index);
            self.live -= 1;
            out.push(payload);
        }
    }

    /// Drop all live timers while retaining node allocation capacity.
    ///
    /// The current tick is preserved: clearing pending work is not time travel.
    pub fn clear(&mut self) {
        self.level0.fill(NIL);
        self.level1.fill(NIL);
        self.level2.fill(NIL);
        self.overflow_head = NIL;
        self.next_overflow_scan = None;
        self.free.clear();
        for (index, node) in self.nodes.iter_mut().enumerate() {
            node.payload = None;
            node.occupied = false;
            node.prev = NIL;
            node.next = NIL;
            node.generation = node.generation.wrapping_add(1).max(1);
            self.free.push(index as u32);
        }
        self.live = 0;
    }

    fn allocate_node(&mut self, payload: T, expiry: u64) -> u32 {
        if let Some(index) = self.free.pop() {
            let node = &mut self.nodes[index as usize];
            node.payload = Some(payload);
            node.expiry = expiry;
            node.prev = NIL;
            node.next = NIL;
            node.occupied = true;
            return index;
        }
        assert!(
            self.nodes.len() < u32::MAX as usize,
            "timer node ids are u32"
        );
        let index = self.nodes.len() as u32;
        self.nodes.push(Node {
            payload: Some(payload),
            expiry,
            generation: 1,
            prev: NIL,
            next: NIL,
            location: Location::Overflow,
            occupied: true,
        });
        index
    }

    fn classify(expiry: u64, current_tick: u64) -> Location {
        let delta = expiry.saturating_sub(current_tick);
        if delta < L1_SPAN {
            Location::Level0((expiry & L0_MASK) as u8)
        } else if delta < L2_SPAN {
            Location::Level1(((expiry >> L1_SHIFT) as u8) & (L1_SIZE as u8 - 1))
        } else if delta < TIMING_WHEEL_SPAN_TICKS {
            Location::Level2(((expiry >> L2_SHIFT) as u8) & (L2_SIZE as u8 - 1))
        } else {
            Location::Overflow
        }
    }

    fn head(&self, location: Location) -> u32 {
        match location {
            Location::Level0(slot) => self.level0[usize::from(slot)],
            Location::Level1(slot) => self.level1[usize::from(slot)],
            Location::Level2(slot) => self.level2[usize::from(slot)],
            Location::Overflow => self.overflow_head,
        }
    }

    fn set_head(&mut self, location: Location, head: u32) {
        match location {
            Location::Level0(slot) => self.level0[usize::from(slot)] = head,
            Location::Level1(slot) => self.level1[usize::from(slot)] = head,
            Location::Level2(slot) => self.level2[usize::from(slot)] = head,
            Location::Overflow => self.overflow_head = head,
        }
    }

    fn link_node(&mut self, index: u32) {
        let expiry = self.nodes[index as usize].expiry;
        let location = Self::classify(expiry, self.current_tick);
        if location == Location::Overflow {
            let scan_tick = expiry.saturating_sub(TIMING_WHEEL_SPAN_TICKS - 1);
            self.next_overflow_scan = Some(
                self.next_overflow_scan
                    .map_or(scan_tick, |old| old.min(scan_tick)),
            );
        }
        let head = self.head(location);
        self.nodes[index as usize].location = location;
        self.nodes[index as usize].prev = NIL;
        self.nodes[index as usize].next = head;
        if head != NIL {
            self.nodes[head as usize].prev = index;
        }
        self.set_head(location, index);
    }

    fn unlink_node(&mut self, index: u32) {
        let location = self.nodes[index as usize].location;
        let prev = self.nodes[index as usize].prev;
        let next = self.nodes[index as usize].next;
        if prev == NIL {
            self.set_head(location, next);
        } else {
            self.nodes[prev as usize].next = next;
        }
        if next != NIL {
            self.nodes[next as usize].prev = prev;
        }
        self.nodes[index as usize].prev = NIL;
        self.nodes[index as usize].next = NIL;
    }

    fn cascade_slot(&mut self, location: Location) {
        let mut cursor = self.head(location);
        self.set_head(location, NIL);
        while cursor != NIL {
            let index = cursor;
            cursor = self.nodes[index as usize].next;
            self.nodes[index as usize].prev = NIL;
            self.nodes[index as usize].next = NIL;
            self.link_node(index);
        }
    }

    fn scan_overflow(&mut self) {
        self.next_overflow_scan = None;
        let mut cursor = self.overflow_head;
        self.overflow_head = NIL;
        while cursor != NIL {
            let index = cursor;
            cursor = self.nodes[index as usize].next;
            self.nodes[index as usize].prev = NIL;
            self.nodes[index as usize].next = NIL;
            self.link_node(index);
        }
    }

    fn retire_detached_node(&mut self, index: u32) {
        let node = &mut self.nodes[index as usize];
        node.occupied = false;
        node.payload = None;
        node.generation = node.generation.wrapping_add(1).max(1);
        self.free.push(index);
    }

    fn valid_node(&self, handle: TimerHandle) -> Option<u32> {
        let index = (handle.0 & u64::from(u32::MAX)) as u32;
        let generation = (handle.0 >> 32) as u32;
        let node = self.nodes.get(index as usize)?;
        if node.occupied && node.generation == generation {
            Some(index)
        } else {
            None
        }
    }
}

impl<T> Default for TimingWheel<T> {
    fn default() -> Self {
        Self::new()
    }
}

fn encode_handle(index: u32, generation: u32) -> TimerHandle {
    TimerHandle((u64::from(generation) << 32) | u64::from(index))
}
