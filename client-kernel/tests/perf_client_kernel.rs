// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Deterministic proof that the chunk light/occlusion hot paths keep
//! bit-identical output while their scratch buffers stop reallocating:
//! a thread-local counting allocator measures allocation calls per pass, so
//! steady-state cycles must allocate exactly the same (output-only) amount
//! every time.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use mc_spine_kernel::{
    CoalescedLightQueue, LightEdit, LightEngine, LightFact, LightKind, LightUpdateMultiWord,
    QueuedLightEdit,
};

struct CountingAlloc;

thread_local! {
    static ALLOC_CALLS: Cell<usize> = const { Cell::new(0) };
    static ALLOC_BYTES: Cell<usize> = const { Cell::new(0) };
}

#[inline]
fn bump(calls: usize, bytes: usize) {
    ALLOC_CALLS.with(|count| count.set(count.get() + calls));
    ALLOC_BYTES.with(|count| count.set(count.get() + bytes));
}

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump(1, layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        bump(1, layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump(1, new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAlloc = CountingAlloc;

/// Cumulative (calls, bytes) allocated on this thread so far.
fn alloc_stats() -> (usize, usize) {
    (
        ALLOC_CALLS.with(Cell::get),
        ALLOC_BYTES.with(Cell::get),
    )
}

const AIR: LightFact = LightFact {
    opacity: 0,
    emission: 0,
    occlusion: [0; 6],
};
const STONE: LightFact = LightFact {
    opacity: 15,
    emission: 0,
    occlusion: [0xf; 6],
};
const TORCH: LightFact = LightFact {
    emission: 14,
    ..AIR
};
const LANTERN: LightFact = LightFact {
    emission: 15,
    ..AIR
};

fn column(height: usize, fill: impl Fn(usize, usize, usize, usize) -> LightFact) -> Vec<LightFact> {
    (0..height * 256)
        .map(|i| fill(i & 15, i >> 8, (i >> 4) & 15, i))
        .collect()
}

/// Index `i` of `cells` answers the `(x, y - min_y, z)` cell as `load_chunk`
/// orders it: y, z, x from the window bottom.
fn cell_index(min_y: i32, x: i32, y: i32, z: i32) -> usize {
    ((y - min_y) as usize) << 8 | ((z & 15) as usize) << 4 | (x & 15) as usize
}

fn edit(x: i32, y: i32, z: i32, fact: LightFact) -> LightEdit {
    LightEdit { x, y, z, fact }
}

/// One reversible edit cycle: every batch changes the window and the cycle as
/// a whole returns it to the loaded state, so the next cycle is identical.
fn edit_cycle(engine: &mut LightEngine, out: &mut Vec<LightUpdateMultiWord>) {
    let batches: [Vec<LightEdit>; 4] = [
        vec![edit(5, 100, 5, STONE), edit(6, 100, 5, LANTERN)],
        vec![edit(20, 700, 5, STONE)],
        vec![edit(5, 100, 5, AIR), edit(6, 100, 5, AIR)],
        vec![edit(20, 700, 5, AIR)],
    ];
    out.clear();
    for batch in &batches {
        out.push(engine.apply_edits_multi_word(batch).unwrap());
    }
}

#[test]
fn perf_client_kernel_light_edit_cycles_bit_identical_zero_scratch_churn() {
    // 64 payload sections force the tall multi-word engine (>62 payload
    // sections cannot fit the u64 masks).
    let (min_y, height) = (0, 1024u32);
    let mut cells = column(height as usize, |_, _, _, _| AIR);
    cells[cell_index(min_y, 4, 100, 4)] = TORCH;
    cells[cell_index(min_y, 20, 700, 4)] = LANTERN;
    let mut engine = LightEngine::new_tall(min_y, height, true).unwrap();
    for cx in [-1, 0, 1] {
        engine.load_chunk(cx, 0, &cells).unwrap();
    }

    // Warmup: grow every scratch buffer once. Each measured cycle must then
    // reproduce the previous cycle bit-for-bit. `previous` starts with the
    // cycle's capacity so the swap never regrows an output vector.
    let mut previous = Vec::with_capacity(4);
    let mut current = Vec::new();
    edit_cycle(&mut engine, &mut current);

    let mut steady_calls = None;
    for round in 0..3 {
        let mark = alloc_stats();
        edit_cycle(&mut engine, &mut current);
        let delta_calls = alloc_stats().0 - mark.0;
        if round > 0 {
            assert_eq!(current, previous, "edit cycle {round} diverged");
        }
        std::mem::swap(&mut current, &mut previous);
        if round > 0 {
            let previous_calls = steady_calls.expect("steady baseline recorded");
            assert_eq!(
                delta_calls, previous_calls,
                "scratch churn changed between steady cycles"
            );
        } else {
            steady_calls = Some(delta_calls);
        }
    }
}

#[test]
fn perf_client_kernel_section_reads_allocate_nothing() {
    let mut engine = LightEngine::new(0, 64, true).unwrap();
    let cells = column(64, |_, _, _, _| AIR);
    engine.load_chunk(0, 0, &cells).unwrap();
    engine.load_chunk(1, 0, &cells).unwrap();

    let read_all = |engine: &LightEngine| {
        for cx in [0, 1] {
            for kind in [LightKind::Sky, LightKind::Block] {
                for index in 0..engine.section_count() {
                    std::hint::black_box(engine.section(cx, 0, kind, index).unwrap());
                }
            }
        }
    };
    read_all(&engine);
    let mark = alloc_stats();
    for _ in 0..16 {
        read_all(&engine);
    }
    let (calls, bytes) = {
        let (c, b) = alloc_stats();
        (c - mark.0, b - mark.1)
    };
    assert_eq!((calls, bytes), (0, 0), "section reads reallocated");
}

#[test]
fn perf_client_kernel_occlusion_queue_cycles_bit_identical_zero_churn() {
    let mut queue = CoalescedLightQueue::new();

    // Deterministic leaf-decay-style burst with heavy last-write-wins
    // overlap, identical on every tick.
    fn push_burst(queue: &mut CoalescedLightQueue) {
        for event in 0..2_300usize {
            let cell = (event % 617) as i32;
            queue.push(QueuedLightEdit {
                x: cell % 49 - 24,
                y: 65 + cell / 49,
                z: (cell * 7) % 61 - 30,
                state_id: if event % 3 == 0 {
                    0
                } else {
                    17 + event as u32
                },
            });
        }
        for event in (0..2_300usize).step_by(7) {
            let cell = (event % 617) as i32;
            queue.push(QueuedLightEdit {
                x: cell % 49 - 24,
                y: 65 + cell / 49,
                z: (cell * 7) % 61 - 30,
                state_id: 900 + event as u32,
            });
        }
    }

    push_burst(&mut queue);
    let reference = queue.drain();
    assert!(!reference.is_empty());

    let mut steady_calls = None;
    for round in 0..3 {
        let mark = alloc_stats();
        push_burst(&mut queue);
        let batches = queue.drain();
        let delta_calls = alloc_stats().0 - mark.0;
        assert_eq!(&batches, &reference, "drain cycle {round} diverged");
        assert!(queue.is_empty());
        if round > 0 {
            let previous = steady_calls.expect("steady baseline recorded");
            assert_eq!(
                delta_calls, previous,
                "scratch churn changed between steady cycles"
            );
        } else {
            steady_calls = Some(delta_calls);
        }
    }
}

#[test]
fn perf_client_kernel_load_chunk_skips_intermediate_vector() {
    // `load_chunk` must validate facts in index order and fail closed before
    // mutating the window, exactly like the collected-vector path did.
    let mut engine = LightEngine::new(0, 32, true).unwrap();
    let bad = LightFact {
        opacity: 16,
        ..AIR
    };
    let mut cells = column(32, |_, _, _, _| AIR);
    cells[cell_index(0, 3, 9, 3)] = bad;
    assert_eq!(
        engine.load_chunk(0, 0, &cells).err(),
        Some(mc_spine_kernel::Error::InvalidArgument)
    );
    assert_eq!(
        engine.section(0, 0, LightKind::Block, 1).err(),
        Some(mc_spine_kernel::Error::OutOfRange)
    );
    cells[cell_index(0, 3, 9, 3)] = STONE;
    engine.load_chunk(0, 0, &cells).unwrap();
    assert_eq!(engine.cells_per_chunk(), 32 * 256);
}
