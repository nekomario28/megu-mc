// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::ring_spsc` — bounded single-producer/single-consumer
//! channels and fixed-capacity ring queues.
//!
//! * [`CachePadded`] — `#[repr(align(64))]` wrapper keeping the producer's
//!   head and the consumer's tail on separate cache lines (no false
//!   sharing on the two hottest words in the system).
//! * [`spsc_u64`] → ([`SpscU64Producer`], [`SpscU64Consumer`]) — a *true*
//!   wait-free, lock-free ring of `AtomicU64` slots under `Acquire` /
//!   `Release` ordering. Both endpoints complete every operation in a
//!   bounded number of instructions with no loops, no CAS, and no
//!   `unsafe` — ideal for audio callbacks, network I/O, and chunk/mesh
//!   wakeup tokens between exactly two threads.
//! * [`spsc_channel`] → ([`SpscProducer`], [`SpscConsumer`]) — the generic
//!   `T` variant. `#![forbid(unsafe_code)]` rules out the classic
//!   `UnsafeCell` slot array, so each slot is an uncontended
//!   `Mutex<Option<T>>`: still bounded, still wait-free in practice (an
//!   uncontended lock is a single atomic exchange), and fully safe.
//! * [`BoundedRing`] — a fixed-capacity contiguous FIFO for
//!   single-thread use: O(1) `push_back` / `pop_front` /
//!   `push_overwrite_oldest`, replacing `Vec::remove(0)` and unbounded
//!   `VecDeque`s in per-thread queues and cap-trimmed histories.
//!
//! Pure `std`, zero dependencies, `unsafe`-free.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// `#[repr(align(64))]` wrapper that isolates `T` on its own cache line,
/// eliminating false sharing between a producer's head and a consumer's
/// tail (and between neighboring ring cursors generally).
#[repr(align(64))]
#[derive(Debug)]
pub struct CachePadded<T>(pub T);

impl<T> CachePadded<T> {
    /// Wrap `value` on a 64-byte boundary.
    pub const fn new(value: T) -> Self {
        Self(value)
    }
}

impl<T> std::ops::Deref for CachePadded<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> std::ops::DerefMut for CachePadded<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

// ---------------------------------------------------------------------------
// Wait-free SPSC u64 ring
// ---------------------------------------------------------------------------

/// Shared state behind [`SpscU64Producer`] / [`SpscU64Consumer`].
///
/// Correctness argument (classic Lamport SPSC):
/// * The producer alone writes `head` and slot values; the consumer alone
///   writes `tail`. Each counter only grows, so `head - tail` (wrapping
///   arithmetic) is a consistent snapshot of occupancy.
/// * The slot store happens *before* `head` is published with `Release`;
///   the consumer reads `head` with `Acquire`, so a visible `head` implies
///   a visible slot value — no torn reads, no `unsafe` required.
/// * A slot is only rewritten after `tail` (read `Acquire`) passes it —
///   i.e. after the consumer's `Release` publish of that slot's index —
///   so producer and consumer never touch the same slot concurrently.
pub struct WaitFreeSpscU64 {
    /// Next write position (producer-owned).
    head: CachePadded<AtomicUsize>,
    /// Next read position (consumer-owned).
    tail: CachePadded<AtomicUsize>,
    buffer: Box<[AtomicU64]>,
    /// `capacity - 1`; capacity is always a power of two.
    mask: usize,
}

/// Construct the two endpoints of a bounded, wait-free `u64` channel with
/// `capacity_pow2` slots (rounded up to the next power of two, minimum 2).
pub fn spsc_u64(capacity_pow2: usize) -> (SpscU64Producer, SpscU64Consumer) {
    let capacity = capacity_pow2.max(2).next_power_of_two();
    let inner = Arc::new(WaitFreeSpscU64 {
        head: CachePadded::new(AtomicUsize::new(0)),
        tail: CachePadded::new(AtomicUsize::new(0)),
        buffer: (0..capacity).map(|_| AtomicU64::new(0)).collect(),
        mask: capacity - 1,
    });
    (
        SpscU64Producer {
            inner: inner.clone(),
        },
        SpscU64Consumer { inner },
    )
}

/// Producer endpoint of [`spsc_u64`]. `Send` — hand it to the producing
/// thread.
pub struct SpscU64Producer {
    inner: Arc<WaitFreeSpscU64>,
}

/// Consumer endpoint of [`spsc_u64`]. `Send` — hand it to the consuming
/// thread.
pub struct SpscU64Consumer {
    inner: Arc<WaitFreeSpscU64>,
}

impl SpscU64Producer {
    /// Non-blocking send: `Err(v)` (value returned) when the ring is full.
    #[inline]
    pub fn try_push(&self, v: u64) -> Result<(), u64> {
        let head = self.inner.head.load(Ordering::Relaxed);
        let tail = self.inner.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) == self.capacity() {
            return Err(v);
        }
        self.inner.buffer[head & self.inner.mask].store(v, Ordering::Relaxed);
        self.inner.head.store(head.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    /// Blocking-style convenience: spin until a slot frees. Prefer
    /// [`SpscU64Producer::try_push`] in latency-sensitive loops.
    pub fn push_blocking(&self, v: u64) {
        let mut v = v;
        loop {
            match self.try_push(v) {
                Ok(()) => return,
                Err(rejected) => {
                    v = rejected;
                    std::hint::spin_loop();
                }
            }
        }
    }

    /// Slots currently occupied (snapshot; may lag a concurrent pop).
    #[inline]
    pub fn len(&self) -> usize {
        self.inner.head.load(Ordering::Acquire)
            .wrapping_sub(self.inner.tail.load(Ordering::Acquire))
    }

    /// Total slots.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.inner.buffer.len()
    }

    /// Whether the ring is currently empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the ring currently has free slots.
    #[inline]
    pub fn is_full(&self) -> bool {
        self.len() == self.capacity()
    }
}

impl SpscU64Consumer {
    /// Non-blocking receive: `None` when empty.
    #[inline]
    pub fn try_pop(&self) -> Option<u64> {
        let tail = self.inner.tail.load(Ordering::Relaxed);
        let head = self.inner.head.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        let v = self.inner.buffer[tail & self.inner.mask].load(Ordering::Relaxed);
        self.inner.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(v)
    }

    /// Blocking-style convenience: spin until a value arrives.
    pub fn pop_blocking(&self) -> u64 {
        loop {
            if let Some(v) = self.try_pop() {
                return v;
            }
            std::hint::spin_loop();
        }
    }

    /// Slots currently occupied (snapshot; may lag a concurrent push).
    #[inline]
    pub fn len(&self) -> usize {
        self.inner.head.load(Ordering::Acquire)
            .wrapping_sub(self.inner.tail.load(Ordering::Acquire))
    }

    /// Whether the ring is currently empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---------------------------------------------------------------------------
// Generic safe SPSC channel
// ---------------------------------------------------------------------------

/// Shared state behind [`SpscProducer`] / [`SpscConsumer`].
///
/// `Vec<UnsafeCell>` is off the table under `#![forbid(unsafe_code)]`, so
/// each slot is a `Mutex<Option<T>>`. The producer is the only writer of
/// a slot and the consumer the only taker — and the head/tail protocol
/// below guarantees they never touch the same slot concurrently — so the
/// mutexes never contend. Poison can only arise from a panic while the
/// guard is held (impossible here: no code runs under the guard besides
/// the `Option` store/take), and `lock_slot` recovers anyway.
struct SpscInner<T> {
    head: CachePadded<AtomicUsize>,
    tail: CachePadded<AtomicUsize>,
    slots: Box<[Mutex<Option<T>>]>,
    mask: usize,
}

fn lock_slot<T>(slot: &Mutex<Option<T>>) -> MutexGuard<'_, Option<T>> {
    slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Construct the endpoints of a bounded generic SPSC channel with
/// `capacity_pow2` slots (rounded up to the next power of two, minimum 2).
/// `T` must be `Send` for the endpoints to cross threads (enforced by
/// the compiler via `Arc`'s auto traits — nothing unsafe is asserted).
pub fn spsc_channel<T>(capacity_pow2: usize) -> (SpscProducer<T>, SpscConsumer<T>) {
    let capacity = capacity_pow2.max(2).next_power_of_two();
    let inner = Arc::new(SpscInner {
        head: CachePadded::new(AtomicUsize::new(0)),
        tail: CachePadded::new(AtomicUsize::new(0)),
        slots: (0..capacity).map(|_| Mutex::new(None)).collect(),
        mask: capacity - 1,
    });
    (
        SpscProducer {
            inner: inner.clone(),
        },
        SpscConsumer { inner },
    )
}

/// Producer endpoint of [`spsc_channel`].
pub struct SpscProducer<T> {
    inner: Arc<SpscInner<T>>,
}

/// Consumer endpoint of [`spsc_channel`].
pub struct SpscConsumer<T> {
    inner: Arc<SpscInner<T>>,
}

impl<T> SpscProducer<T> {
    /// Non-blocking send: `Err(v)` (value returned) when full.
    pub fn try_push(&self, v: T) -> Result<(), T> {
        let head = self.inner.head.load(Ordering::Relaxed);
        let tail = self.inner.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) == self.capacity() {
            return Err(v);
        }
        // Publish through the slot mutex first, then advance head with
        // Release so a consumer that observes the new head is guaranteed
        // to observe the stored value.
        *lock_slot(&self.inner.slots[head & self.inner.mask]) = Some(v);
        self.inner.head.store(head.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    /// Total slots.
    pub fn capacity(&self) -> usize {
        self.inner.slots.len()
    }

    /// Slots currently occupied (snapshot).
    pub fn len(&self) -> usize {
        self.inner.head.load(Ordering::Acquire)
            .wrapping_sub(self.inner.tail.load(Ordering::Acquire))
    }

    /// Whether the ring is currently empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the ring currently has no free slots.
    pub fn is_full(&self) -> bool {
        self.len() == self.capacity()
    }
}

impl<T> SpscConsumer<T> {
    /// Non-blocking receive: `None` when empty.
    pub fn try_pop(&self) -> Option<T> {
        let tail = self.inner.tail.load(Ordering::Relaxed);
        let head = self.inner.head.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        let v = lock_slot(&self.inner.slots[tail & self.inner.mask]).take();
        self.inner.tail.store(tail.wrapping_add(1), Ordering::Release);
        v
    }

    /// Slots currently occupied (snapshot).
    pub fn len(&self) -> usize {
        self.inner.head.load(Ordering::Acquire)
            .wrapping_sub(self.inner.tail.load(Ordering::Acquire))
    }

    /// Whether the ring is currently empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---------------------------------------------------------------------------
// BoundedRing — single-thread fixed-capacity FIFO
// ---------------------------------------------------------------------------

/// Fixed-capacity contiguous ring buffer for single-thread use.
///
/// O(1) `push_back`/`pop_front` with no shifting — the drop-in answer to
/// `Vec::remove(0)` hot paths and unbounded growth in per-tick queues and
/// rolling histories. `push_overwrite_oldest` implements the "keep the
/// newest N" policy in one branch.
pub struct BoundedRing<T> {
    /// `buf.len() == capacity`; `None` marks free slots.
    buf: Vec<Option<T>>,
    /// Index of the oldest element (pop end).
    head: usize,
    len: usize,
}

impl<T> BoundedRing<T> {
    /// Empty ring holding at most `capacity` elements.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: std::iter::repeat_with(|| None).take(capacity).collect(),
            head: 0,
            len: 0,
        }
    }

    /// Append `v` at the back; `Err(v)` (value returned) when full.
    pub fn push_back(&mut self, v: T) -> Result<(), T> {
        if self.len == self.buf.len() {
            return Err(v);
        }
        let idx = (self.head + self.len) % self.buf.len().max(1);
        self.buf[idx] = Some(v);
        self.len += 1;
        Ok(())
    }

    /// Remove and return the oldest element, `None` when empty.
    pub fn pop_front(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let v = self.buf[self.head].take();
        self.head = (self.head + 1) % self.buf.len().max(1);
        self.len -= 1;
        v
    }

    /// Append `v`, evicting the oldest element first when full. The
    /// classic bounded-history primitive: the ring always keeps the
    /// newest `capacity` items.
    pub fn push_overwrite_oldest(&mut self, v: T) {
        if self.buf.is_empty() {
            return;
        }
        if self.len == self.buf.len() {
            self.buf[self.head] = Some(v);
            self.head = (self.head + 1) % self.buf.len();
        } else {
            // Capacity free: place at the back without evicting.
            let idx = (self.head + self.len) % self.buf.len();
            self.buf[idx] = Some(v);
            self.len += 1;
        }
    }

    /// Shared reference to the oldest element.
    pub fn front(&self) -> Option<&T> {
        if self.len == 0 {
            return None;
        }
        self.buf[self.head].as_ref()
    }

    /// Shared reference to the newest element.
    pub fn back(&self) -> Option<&T> {
        if self.len == 0 {
            return None;
        }
        let idx = (self.head + self.len - 1) % self.buf.len();
        self.buf[idx].as_ref()
    }

    /// Number of stored elements.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the ring is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the ring is at capacity.
    pub fn is_full(&self) -> bool {
        self.len == self.buf.len() && self.len != 0
    }

    /// Maximum element count.
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Drop all elements (keeps the allocation).
    pub fn clear(&mut self) {
        for slot in &mut self.buf {
            *slot = None;
        }
        self.head = 0;
        self.len = 0;
    }
}
