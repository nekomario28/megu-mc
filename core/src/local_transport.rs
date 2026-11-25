// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::local_transport` — a zero-socket, zero-copy loopback
//! transport for integrated singleplayer.
//!
//! When the vanilla server runs in the same process as the GPU client
//! (integrated singleplayer, LAN host, modded local play), the classic
//! pipeline still serializes every chunk packet to bytes, pushes it
//! through a TCP loopback socket, copies it through two kernel buffers,
//! and deserializes it on the other side. That work buys nothing: both
//! ends share one address space.
//!
//! [`local_pair`] replaces the whole stack with two lock-free single
//! producer/single consumer lanes built directly on the canonical
//! [`crate::ring_spsc::WaitFreeSpscU64`] token rings. Each lane couples a
//! `u64` index ring with a shared *slot table* of `Mutex<Option<Arc<T>>>`
//! cells: the producer parks one `Arc<T>` in a free slot and publishes
//! only the slot index; the consumer takes the same `Arc` back out —
//! pointer, allocation and payload included. There are **zero sockets,
//! zero syscalls, zero serialization, zero compression and zero payload
//! copies**: `Arc::ptr_eq` on the received handle proves the object is
//! the sender's, unchanged.
//!
//! Backpressure is the ring capacity itself: a fast server cannot run
//! the client's memory away because `try_send` fails with
//! [`TrySendError::Full`] the instant every payload slot is occupied, and
//! freed slot indices recycle through a second `WaitFreeSpscU64` ring
//! flowing back from consumer to producer with the same wait-free
//! guarantees.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::ring_spsc::{spsc_u64, SpscU64Consumer, SpscU64Producer};

/// Shared payload slots for one direction of a [`local_pair`] lane.
///
/// Slot `i` holds `None` while its index sits in the free ring or in
/// flight, and `Some(Arc<T>)` while its index sits in the data ring.
struct SlotTable<T> {
    slots: Box<[Mutex<Option<Arc<T>>>]>,
}

impl<T> SlotTable<T> {
    fn slot(&self, index: usize) -> &Mutex<Option<Arc<T>>> {
        &self.slots[index]
    }
}

fn lock_slot<T>(slot: &Mutex<Option<Arc<T>>>) -> MutexGuard<'_, Option<Arc<T>>> {
    slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Why a [`LocalEndpoint::try_send`] did not deliver. The message comes
/// back in every variant so the caller can retry, drop, or reclaim it.
#[derive(Debug)]
pub enum TrySendError<T> {
    /// Every payload slot is occupied — apply backpressure, retry later.
    Full(Arc<T>),
    /// The peer endpoint was dropped; the message cannot be delivered.
    Disconnected(Arc<T>),
}

impl<T> TrySendError<T> {
    /// Recover the undelivered message.
    pub fn into_message(self) -> Arc<T> {
        match self {
            Self::Full(msg) | Self::Disconnected(msg) => msg,
        }
    }
}

impl<T> fmt::Display for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => f.write_str("local transport ring is full"),
            Self::Disconnected(_) => f.write_str("local transport peer disconnected"),
        }
    }
}

impl<T: fmt::Debug> std::error::Error for TrySendError<T> {}

/// Send half of one lane: pops a free slot index, parks the `Arc` in the
/// slot table, publishes the index on the data ring.
struct TxHalf<T> {
    data: SpscU64Producer,
    free: SpscU64Consumer,
    slots: Arc<SlotTable<T>>,
}

/// Receive half of one lane: pops a slot index off the data ring, takes
/// the `Arc` out of the slot table, returns the index to the free ring.
struct RxHalf<T> {
    data: SpscU64Consumer,
    free: SpscU64Producer,
    slots: Arc<SlotTable<T>>,
}

fn channel<T>(capacity_pow2: usize) -> (TxHalf<T>, RxHalf<T>) {
    let capacity = capacity_pow2.max(2).next_power_of_two();
    let (data_prod, data_cons) = spsc_u64(capacity);
    let (free_prod, free_cons) = spsc_u64(capacity);
    // Every slot starts free. The free ring is exactly `capacity` deep,
    // so all of these pushes succeed without spinning.
    for index in 0..capacity {
        debug_assert!(free_prod.try_push(index as u64).is_ok());
    }
    let slots = Arc::new(SlotTable {
        slots: (0..capacity).map(|_| Mutex::new(None)).collect(),
    });
    (
        TxHalf {
            data: data_prod,
            free: free_cons,
            slots: slots.clone(),
        },
        RxHalf {
            data: data_cons,
            free: free_prod,
            slots,
        },
    )
}

/// One end of a full-duplex in-process transport.
///
/// `Tx` is the payload this endpoint sends and `Rx` the payload it
/// receives: `local_pair::<S2C, C2S>` returns
/// `(LocalEndpoint<S2C, C2S>, LocalEndpoint<C2S, S2C>)` — the integrated
/// server holds the first, the GPU client the second. Payloads move as
/// `Arc<T>`, so a chunk snapshot, entity batch or mod packet crosses the
/// lane as one pointer with no serialization step at all.
///
/// The endpoint is `Send` when both payload types are `Send + Sync` (the
/// `Arc`-in-`Mutex` slot tables require it); hand one endpoint to each
/// thread.
pub struct LocalEndpoint<Tx, Rx> {
    tx: TxHalf<Tx>,
    rx: RxHalf<Rx>,
    /// This endpoint's liveness, cleared on `Drop`.
    alive: Arc<AtomicBool>,
    /// The peer endpoint's liveness (its `alive` flag, shared).
    peer_alive: Arc<AtomicBool>,
    sent: AtomicU64,
    received: AtomicU64,
    bytes_saved: AtomicU64,
}

/// Construct the two ends of a full-duplex in-process transport with
/// `capacity_pow2` payload slots per direction (rounded up to a power of
/// two, minimum two — the [`crate::ring_spsc::spsc_u64`] convention).
///
/// The returned endpoints are mirror images: whatever the first
/// `try_send`s as `S2C`, the second `try_recv`s; whatever the second
/// `try_send`s as `C2S`, the first `try_recv`s.
#[must_use]
pub fn local_pair<S2C, C2S>(
    capacity_pow2: usize,
) -> (LocalEndpoint<S2C, C2S>, LocalEndpoint<C2S, S2C>) {
    let (s2c_tx, s2c_rx) = channel::<S2C>(capacity_pow2);
    let (c2s_tx, c2s_rx) = channel::<C2S>(capacity_pow2);
    let alive_server = Arc::new(AtomicBool::new(true));
    let alive_client = Arc::new(AtomicBool::new(true));
    (
        LocalEndpoint {
            tx: s2c_tx,
            rx: c2s_rx,
            alive: alive_server.clone(),
            peer_alive: alive_client.clone(),
            sent: AtomicU64::new(0),
            received: AtomicU64::new(0),
            bytes_saved: AtomicU64::new(0),
        },
        LocalEndpoint {
            tx: c2s_tx,
            rx: s2c_rx,
            alive: alive_client,
            peer_alive: alive_server,
            sent: AtomicU64::new(0),
            received: AtomicU64::new(0),
            bytes_saved: AtomicU64::new(0),
        },
    )
}

impl<Tx, Rx> LocalEndpoint<Tx, Rx> {
    /// Non-blocking send of an already-shared payload. On success the
    /// *same* `Arc` — same pointer, same allocation — becomes receivable
    /// by the peer; nothing is cloned, copied, or encoded.
    ///
    /// `Err(TrySendError::Full(msg))` when every payload slot is occupied
    /// (backpressure), `Err(TrySendError::Disconnected(msg))` once the
    /// peer endpoint is dropped.
    pub fn try_send(&self, msg: Arc<Tx>) -> Result<(), TrySendError<Tx>> {
        if !self.peer_alive.load(Ordering::Acquire) {
            return Err(TrySendError::Disconnected(msg));
        }
        let Some(index) = self.tx.free.try_pop() else {
            return Err(TrySendError::Full(msg));
        };
        let slot_index = index as usize;
        *lock_slot(self.tx.slots.slot(slot_index)) = Some(msg);
        let pushed = self.tx.data.try_push(index);
        // A popped free slot provably implies a free data-ring slot
        // (data + free + in-flight == capacity), so this cannot fire; if
        // it ever did, the payload is reclaimed and only the index leaks.
        debug_assert!(pushed.is_ok());
        if pushed.is_err() {
            let msg = lock_slot(self.tx.slots.slot(slot_index))
                .take()
                .expect("slot just populated by this thread");
            return Err(TrySendError::Full(msg));
        }
        self.sent.fetch_add(1, Ordering::Relaxed);
        self.bytes_saved
            .fetch_add(size_of::<Tx>() as u64, Ordering::Relaxed);
        Ok(())
    }

    /// Non-blocking receive: the sender's `Arc`, unchanged
    /// (`Arc::ptr_eq` against the sent handle is `true`), or `None` when
    /// the lane is momentarily empty.
    pub fn try_recv(&self) -> Option<Arc<Rx>> {
        let index = self.rx.data.try_pop()?;
        let msg = lock_slot(self.rx.slots.slot(index as usize))
            .take()
            .expect("published slot index always carries a payload");
        // Returning the index cannot fail: the popped data slot just
        // proved one slot exists outside the free ring.
        let pushed = self.rx.free.try_push(index);
        debug_assert!(pushed.is_ok());
        self.received.fetch_add(1, Ordering::Relaxed);
        Some(msg)
    }

    /// Drain every pending payload into `out` in FIFO order; returns the
    /// number drained.
    pub fn drain_into(&self, out: &mut Vec<Arc<Rx>>) -> usize {
        let start = out.len();
        while let Some(msg) = self.try_recv() {
            out.push(msg);
        }
        out.len() - start
    }

    /// Payloads this endpoint has sent.
    #[inline]
    #[must_use]
    pub fn sent_count(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// Payloads this endpoint has received.
    #[inline]
    #[must_use]
    pub fn received_count(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    /// Running estimate of the payload bytes a socket transport would
    /// have serialized, compressed and copied for this endpoint's sent
    /// traffic (`sent_count × size_of::<Tx>()` — a structural estimate
    /// that deliberately ignores heap-indirected payload contents).
    #[inline]
    #[must_use]
    pub fn bytes_saved_estimate(&self) -> u64 {
        self.bytes_saved.load(Ordering::Relaxed)
    }

    /// `true` while the peer endpoint is alive. When this flips `false`,
    /// `try_send` starts failing with [`TrySendError::Disconnected`] and
    /// any already-queued payloads remain receivable until drained.
    #[inline]
    #[must_use]
    pub fn peer_connected(&self) -> bool {
        self.peer_alive.load(Ordering::Acquire)
    }

    /// Total payload slots on this endpoint's send lane.
    #[inline]
    #[must_use]
    pub fn send_capacity(&self) -> usize {
        self.tx.data.capacity()
    }

    /// Payloads currently queued in this endpoint's receive lane
    /// (snapshot; may lag a concurrent send).
    #[inline]
    #[must_use]
    pub fn recv_len(&self) -> usize {
        self.rx.data.len()
    }
}

impl<Tx, Rx> Drop for LocalEndpoint<Tx, Rx> {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
    }
}
