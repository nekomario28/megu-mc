//! Standalone 1.21.1 section-palette differential benchmark for Issue #41.

mod prototype;
mod wire;

use prototype::PrototypeSection;
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use wire::CELLS;

// The generated file contains precisely the production implementation before
// its optional worldgen benchmark. No production source is copied into git.
#[allow(dead_code)]
mod baseline {
    use crate::{write_palette, write_varint};
    include!(concat!(env!("OUT_DIR"), "/production_palette.rs"));
}

fn write_varint(out: &mut Vec<u8>, value: i64) {
    wire::put_varint(out, value as u64);
}

fn write_palette(
    out: &mut Vec<u8>,
    values: &[i64],
    min_bits: usize,
    max_bits: usize,
    global_bits: usize,
) {
    assert_eq!((min_bits, max_bits, global_bits), (1, 3, 6));
    wire::write_biomes(out, values);
}

fn is_air(state: u32) -> bool {
    baseline::is_air_state(state)
}

fn opaque(state: u32) -> bool {
    !is_air(state) && state % 10 != 7
}

fn hidden(state: u32) -> bool {
    state == 50
}

fn production(cells: &[u32; CELLS]) -> baseline::PalettedSection {
    baseline::PalettedSection::pack(cells, &opaque, &hidden)
}

struct TrackingAllocator;
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let result = System.alloc(layout);
        if !result.is_null() {
            ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }

    unsafe fn realloc(&self, ptr: *mut u8, old: Layout, size: usize) -> *mut u8 {
        let result = System.realloc(ptr, old, size);
        if !result.is_null() {
            ALLOC_BYTES.fetch_add(size as u64, Ordering::Relaxed);
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        result
    }
}

fn allocated() -> (u64, u64) {
    (
        ALLOC_BYTES.load(Ordering::Relaxed),
        ALLOC_COUNT.load(Ordering::Relaxed),
    )
}

fn delta(before: (u64, u64)) -> (u64, u64) {
    let now = allocated();
    (now.0 - before.0, now.1 - before.1)
}

type Edit = (usize, u32);

struct Workload {
    name: &'static str,
    initial: Box<[u32; CELLS]>,
    edits: Vec<Edit>,
    checkpoints: Vec<(usize, u8, u8)>,
}

fn state(index: usize) -> u32 {
    1 + index as u32
}

fn workloads() -> Vec<Workload> {
    let mut cases = vec![
        Workload {
            name: "air",
            initial: Box::new([0; CELLS]),
            edits: vec![],
            checkpoints: vec![],
        },
        Workload {
            name: "stone",
            initial: Box::new([1; CELLS]),
            edits: vec![],
            checkpoints: vec![],
        },
        Workload {
            name: "fill_stone",
            initial: Box::new([0; CELLS]),
            edits: (0..CELLS).map(|cell| (cell, 1)).collect(),
            checkpoints: vec![(CELLS, 4, 0)],
        },
    ];
    for cardinality in [2, 16, 256, 512] {
        let edits = (1..cardinality)
            .map(|i| (i - 1, state(i)))
            .collect::<Vec<_>>();
        let checkpoints = [
            (2, 4),
            (16, 4),
            (17, 5),
            (32, 5),
            (33, 6),
            (64, 6),
            (65, 7),
            (128, 7),
            (129, 8),
            (256, 8),
            (257, 15),
            (512, 15),
        ]
        .into_iter()
        .filter(|(n, _)| *n <= cardinality)
        .map(|(n, bits)| (n - 1, bits, bits))
        .collect();
        let name = match cardinality {
            2 => "expand_2",
            16 => "expand_16",
            256 => "expand_256",
            _ => "expand_512",
        };
        cases.push(Workload {
            name,
            initial: Box::new([0; CELLS]),
            edits,
            checkpoints,
        });
    }

    let initial = Box::new(std::array::from_fn(|i| state(i % 256)));
    let edits = (0..4)
        .flat_map(|pass| {
            (0..CELLS).map(move |cell| {
                (
                    cell,
                    state(cell % 256 + if pass % 2 == 0 { 256 } else { 0 }),
                )
            })
        })
        .collect();
    cases.push(Workload {
        name: "churn_256",
        initial,
        edits,
        checkpoints: (1..=4).map(|pass| (pass * CELLS, 15, 8)).collect(),
    });

    let initial = Box::new(std::array::from_fn(|i| state(i % 32)));
    let mut edits = (0..CELLS)
        .filter(|&i| i % 32 != 0)
        .map(|i| (i, 1))
        .collect::<Vec<_>>();
    let collapsed = edits.len();
    edits.extend((1..32).map(|i| (i, state(i))));
    cases.push(Workload {
        name: "fragment_32",
        initial,
        edits,
        checkpoints: vec![(collapsed, 5, 0), (collapsed + 31, 5, 5)],
    });

    let edits = (0..4)
        .flat_map(|pass| (0..CELLS).map(move |cell| (cell, if pass % 2 == 0 { 1 } else { 0 })))
        .collect();
    cases.push(Workload {
        name: "switch_air_stone",
        initial: Box::new([0; CELLS]),
        edits,
        checkpoints: (1..=4).map(|pass| (pass * CELLS, 4, 0)).collect(),
    });
    cases
}

fn assert_equal(
    label: &str,
    current: &[u32; CELLS],
    base: &baseline::PalettedSection,
    trial: &PrototypeSection,
    biomes: &[i64; 64],
) -> (usize, bool) {
    let mut old = [0; CELLS];
    base.unpack_into(&mut old);
    assert_eq!(&old, current, "{label}: production cells");
    assert_eq!(trial.cells(), *current, "{label}: prototype cells");
    trial.assert_metadata(current);
    assert_eq!(base.non_air(), trial.non_air(), "{label}: non-air counts");
    let mut canonical = Vec::new();
    production(current).write_wire(biomes, &mut canonical);
    let mut new_wire = Vec::new();
    trial.write_wire(biomes, &mut new_wire);
    assert_eq!(canonical, new_wire, "{label}: canonical wire identity");
    let (decoded, decoded_biomes, count) = wire::decode(&canonical);
    assert_eq!(decoded, *current, "{label}: decoded state identity");
    assert_eq!(decoded_biomes, *biomes, "{label}: decoded biomes");
    assert_eq!(count, base.non_air());

    let mut raw = Vec::new();
    base.write_wire(biomes, &mut raw);
    assert_eq!(
        wire::decode(&raw).0,
        *current,
        "{label}: resident baseline wire decodes"
    );
    (canonical.len(), raw != canonical)
}

fn exercise(
    case: &Workload,
    check_checkpoints: bool,
) -> (baseline::PalettedSection, PrototypeSection, [u32; CELLS]) {
    let biomes = [3i64; 64];
    let mut current = *case.initial;
    let mut base = production(&current);
    let mut trial = PrototypeSection::from_cells(&current);
    assert_equal(case.name, &current, &base, &trial, &biomes);
    for (step, &(cell, value)) in case.edits.iter().enumerate() {
        base.set(cell, value, &opaque, &hidden);
        trial.set(cell, value);
        current[cell] = value;
        if check_checkpoints {
            if let Some(&(_, base_bits, trial_bits)) =
                case.checkpoints.iter().find(|&&(n, _, _)| n == step + 1)
            {
                assert_eq!(
                    base.palette_bits(),
                    base_bits,
                    "{} step {} baseline width",
                    case.name,
                    step + 1
                );
                assert_eq!(
                    trial.bits(),
                    trial_bits,
                    "{} step {} prototype width",
                    case.name,
                    step + 1
                );
                assert_equal(case.name, &current, &base, &trial, &biomes);
            }
        }
    }
    assert_equal(case.name, &current, &base, &trial, &biomes);
    (base, trial, current)
}

fn get_ns_baseline(section: &baseline::PalettedSection) -> f64 {
    let now = Instant::now();
    let mut sink = 0u64;
    for _ in 0..64 {
        for cell in 0..CELLS {
            sink = sink.wrapping_add(black_box(section).get(black_box(cell)) as u64);
        }
    }
    black_box(sink);
    now.elapsed().as_nanos() as f64 / (64 * CELLS) as f64
}

fn get_ns_prototype(section: &PrototypeSection) -> f64 {
    let now = Instant::now();
    let mut sink = 0u64;
    for _ in 0..64 {
        for cell in 0..CELLS {
            sink = sink.wrapping_add(black_box(section).get(black_box(cell)) as u64);
        }
    }
    black_box(sink);
    now.elapsed().as_nanos() as f64 / (64 * CELLS) as f64
}

fn time_build_baseline(cells: &[u32; CELLS], rounds: usize) -> f64 {
    let start = Instant::now();
    let mut sink = 0u64;
    for _ in 0..rounds {
        let s = production(black_box(cells));
        sink += black_box(s).non_air() as u64;
    }
    black_box(sink);
    start.elapsed().as_nanos() as f64 / rounds as f64
}

fn time_build_prototype(cells: &[u32; CELLS], rounds: usize) -> f64 {
    let start = Instant::now();
    let mut sink = 0u64;
    for _ in 0..rounds {
        let s = PrototypeSection::from_cells(black_box(cells));
        sink += black_box(s).non_air() as u64;
    }
    black_box(sink);
    start.elapsed().as_nanos() as f64 / rounds as f64
}

fn benchmark_case(case: &Workload) {
    let (checked_base, checked_trial, states) = exercise(case, true);
    let initial = &*case.initial;
    // Cold-build measures the actual *final* state distribution, not the
    // uniform starting state used by incremental-expansion scenarios.
    let build_base = time_build_baseline(&states, 64);
    let build_trial = time_build_prototype(&states, 64);
    let before = allocated();
    let built_base = production(&states);
    let (build_alloc_base, build_calls_base) = delta(before);
    drop(built_base);
    let before = allocated();
    let built_trial = PrototypeSection::from_cells(&states);
    let (build_alloc_trial, build_calls_trial) = delta(before);
    drop(built_trial);
    let (set_base, alloc_base, ops_base) = {
        let mut section = production(initial);
        let mark = allocated();
        let start = Instant::now();
        for &(cell, value) in &case.edits {
            black_box(&mut section).set(cell, value, &opaque, &hidden);
        }
        let ns = start.elapsed().as_nanos() as f64;
        let (bytes, calls) = delta(mark);
        assert_eq!(section, checked_base);
        (ns, bytes, calls)
    };
    let (set_trial, alloc_trial, ops_trial) = {
        let mut section = PrototypeSection::from_cells(initial);
        let mark = allocated();
        let start = Instant::now();
        for &(cell, value) in &case.edits {
            black_box(&mut section).set(cell, value);
        }
        let ns = start.elapsed().as_nanos() as f64;
        let (bytes, calls) = delta(mark);
        assert_eq!(section.cells(), states);
        (ns, bytes, calls)
    };
    let (wire_len, resident_wire_difference) =
        assert_equal(case.name, &states, &checked_base, &checked_trial, &[3; 64]);
    let denom = case.edits.len().max(1) as f64;
    println!(
        "case={} edits={} build_ns_base={:.1} build_ns_proto={:.1} build_alloc_bytes_base={} build_alloc_bytes_proto={} build_alloc_calls_base={} build_alloc_calls_proto={} set_ns_base={} set_ns_proto={} get_ns_base={:.2} get_ns_proto={:.2} resident_bytes_base={} resident_bytes_proto={} alloc_bytes_set_base={} alloc_bytes_set_proto={} alloc_calls_set_base={} alloc_calls_set_proto={} bits_base={} bits_proto={} canonical_wire_bytes={} baseline_resident_wire_noncanonical={}",
        case.name, case.edits.len(), build_base, build_trial,
        build_alloc_base, build_alloc_trial, build_calls_base, build_calls_trial,
        if case.edits.is_empty() { "NA".to_owned() } else { format!("{:.1}", set_base / denom) },
        if case.edits.is_empty() { "NA".to_owned() } else { format!("{:.1}", set_trial / denom) },
        get_ns_baseline(&checked_base), get_ns_prototype(&checked_trial),
        checked_base.approx_bytes(), checked_trial.approx_bytes(),
        alloc_base, alloc_trial, ops_base, ops_trial,
        checked_base.palette_bits(), checked_trial.bits(), wire_len,
        resident_wire_difference,
    );

    if !case.checkpoints.is_empty() {
        let mut base = production(initial);
        let mut trial = PrototypeSection::from_cells(initial);
        let mut current = *initial;
        for (step, &(cell, value)) in case.edits.iter().enumerate() {
            base.set(cell, value, &opaque, &hidden);
            trial.set(cell, value);
            current[cell] = value;
            if case.checkpoints.iter().any(|&(at, _, _)| at == step + 1) {
                let (bytes, _) = assert_equal("checkpoint", &current, &base, &trial, &[3; 64]);
                println!(
                    "checkpoint={} step={} bits_base={} bits_proto={} resident_bytes_base={} resident_bytes_proto={} canonical_wire_bytes={}",
                    case.name, step + 1, base.palette_bits(), trial.bits(),
                    base.approx_bytes(), trial.approx_bytes(), bytes,
                );
            }
        }
    }
}

fn batch_cells() -> Vec<Box<[u32; CELLS]>> {
    (0..384)
        .map(|section| {
            let distinct = [1, 1, 2, 16, 256, 512][section % 6];
            Box::new(std::array::from_fn(|cell| {
                if distinct == 1 {
                    (section % 2) as u32
                } else {
                    state((cell + section * 7) % distinct)
                }
            }))
        })
        .collect()
}

fn batch_biomes() -> [i64; 64] {
    std::array::from_fn(|index| (index % 7) as i64)
}

fn rss_kib() -> Option<usize> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn benchmark_batch() {
    let cells = batch_cells();
    let biomes = batch_biomes();
    let before = allocated();
    let start = Instant::now();
    let base: Vec<_> = cells.iter().map(|s| production(s)).collect();
    let cold_base_ns = start.elapsed().as_nanos() as f64 / cells.len() as f64;
    let (cold_alloc_bytes_base, cold_alloc_calls_base) = delta(before);
    let before = allocated();
    let start = Instant::now();
    let trial: Vec<_> = cells
        .iter()
        .map(|s| PrototypeSection::from_cells(s))
        .collect();
    let cold_trial_ns = start.elapsed().as_nanos() as f64 / cells.len() as f64;
    let (cold_alloc_bytes_proto, cold_alloc_calls_proto) = delta(before);
    let footprint_base = base.iter().map(|s| s.approx_bytes()).sum::<usize>();
    let footprint_trial = trial.iter().map(|s| s.approx_bytes()).sum::<usize>();
    let rss = rss_kib();
    let mut encoded = Vec::with_capacity(cells.len());
    let mut sum_bytes = 0;
    for i in 0..cells.len() {
        let (size, stale) = assert_equal("batch", &cells[i], &base[i], &trial[i], &biomes);
        assert!(!stale);
        sum_bytes += size;
        let mut wire = Vec::new();
        base[i].write_wire(&biomes, &mut wire);
        encoded.push(wire);
    }

    let rounds = 20;
    let start = Instant::now();
    let mut bytes_base = 0;
    let mut buffer = Vec::with_capacity(9000);
    for _ in 0..rounds {
        for section in &base {
            buffer.clear();
            black_box(section).write_wire(&biomes, &mut buffer);
            bytes_base += black_box(buffer.len());
        }
    }
    let wire_base_ns = start.elapsed().as_nanos() as f64 / (rounds * base.len()) as f64;
    let start = Instant::now();
    let mut bytes_trial = 0;
    for _ in 0..rounds {
        for section in &trial {
            buffer.clear();
            black_box(section).write_wire(&biomes, &mut buffer);
            bytes_trial += black_box(buffer.len());
        }
    }
    let wire_trial_ns = start.elapsed().as_nanos() as f64 / (rounds * trial.len()) as f64;
    assert_eq!(bytes_base, bytes_trial);

    let start = Instant::now();
    let mut checksum = 0u64;
    for _ in 0..rounds {
        for packet in &encoded {
            let (states, _, count) = wire::decode(black_box(packet));
            checksum = checksum.wrapping_add(states[4095] as u64 + count as u64);
        }
    }
    black_box(checksum);
    let decode_ns = start.elapsed().as_nanos() as f64 / (rounds * encoded.len()) as f64;

    let start = Instant::now();
    for (i, packet) in encoded.iter().enumerate() {
        let (states, decoded_biomes, _) = wire::decode(packet);
        let restored_base = production(&states);
        let restored_trial = PrototypeSection::from_cells(&states);
        assert_equal(
            "reload",
            &states,
            &restored_base,
            &restored_trial,
            &decoded_biomes,
        );
        assert_eq!(states, *cells[i]);
    }
    let reload_ns = start.elapsed().as_nanos() as f64 / encoded.len() as f64;

    // Cold snapshots and a steady-state edit pass exercise two distinct paths.
    let mut edited_base = base;
    let mut edited_trial = trial;
    let mut edited_cells = cells;
    let before = allocated();
    let start = Instant::now();
    for i in 0..edited_cells.len() {
        for k in 0..96 {
            let cell = (i * 97 + k * 41) % CELLS;
            let state = state((i * 29 + k * 13) % 255);
            edited_base[i].set(cell, state, &opaque, &hidden);
            edited_cells[i][cell] = state;
        }
    }
    let steady_edit_base_ns = start.elapsed().as_nanos() as f64 / (384 * 96) as f64;
    let (steady_alloc_bytes_base, steady_alloc_calls_base) = delta(before);
    let before = allocated();
    let start = Instant::now();
    for (i, section) in edited_trial.iter_mut().enumerate() {
        for k in 0..96 {
            let cell = (i * 97 + k * 41) % CELLS;
            let state = state((i * 29 + k * 13) % 255);
            section.set(cell, state);
        }
    }
    let steady_edit_proto_ns = start.elapsed().as_nanos() as f64 / (384 * 96) as f64;
    let (steady_alloc_bytes_proto, steady_alloc_calls_proto) = delta(before);
    let mut dirty_wire_bytes = 0;
    let mut dirty_raw_mismatches = 0;
    for i in 0..edited_cells.len() {
        let (size, mismatched) = assert_equal(
            "batch steady edit",
            &edited_cells[i],
            &edited_base[i],
            &edited_trial[i],
            &biomes,
        );
        dirty_wire_bytes += size;
        dirty_raw_mismatches += usize::from(mismatched);
    }
    let start = Instant::now();
    let mut buffer = Vec::with_capacity(9000);
    let mut bytes_a = 0;
    for cells in &edited_cells {
        buffer.clear();
        production(black_box(cells)).write_wire(&biomes, &mut buffer);
        bytes_a += black_box(buffer.len());
    }
    let dirty_wire_base_ns = start.elapsed().as_nanos() as f64 / 384.0;
    let start = Instant::now();
    let mut bytes_b = 0;
    for section in &edited_trial {
        buffer.clear();
        black_box(section).write_wire(&biomes, &mut buffer);
        bytes_b += black_box(buffer.len());
    }
    let dirty_wire_proto_ns = start.elapsed().as_nanos() as f64 / 384.0;
    assert_eq!(bytes_a, bytes_b);
    println!(
        "batch sections=384 cold_build_ns_base={cold_base_ns:.1} cold_build_ns_proto={cold_trial_ns:.1} cold_alloc_bytes_base={cold_alloc_bytes_base} cold_alloc_bytes_proto={cold_alloc_bytes_proto} cold_alloc_calls_base={cold_alloc_calls_base} cold_alloc_calls_proto={cold_alloc_calls_proto} resident_bytes_base={footprint_base} resident_bytes_proto={footprint_trial} process_rss_kib_after_build={} canonical_wire_bytes={sum_bytes} serialize_ns_base={wire_base_ns:.1} serialize_ns_proto={wire_trial_ns:.1} decode_ns_per_section={decode_ns:.1} verified_reload_ns_per_section={reload_ns:.1} steady_edit_ns_base={steady_edit_base_ns:.1} steady_edit_ns_proto={steady_edit_proto_ns:.1} steady_alloc_bytes_base={steady_alloc_bytes_base} steady_alloc_bytes_proto={steady_alloc_bytes_proto} steady_alloc_calls_base={steady_alloc_calls_base} steady_alloc_calls_proto={steady_alloc_calls_proto} dirty_canonical_encode_ns_base={dirty_wire_base_ns:.1} dirty_canonical_encode_ns_proto={dirty_wire_proto_ns:.1} steady_canonical_wire_bytes={dirty_wire_bytes} stale_resident_baseline_wires={dirty_raw_mismatches}",
        rss.map_or_else(|| "NA".to_owned(), |v| v.to_string()),
    );
}

fn main() {
    println!("benchmark=issue41 palette_bench v1 baseline_compiled_from=src/paletted.rs host_arch={} rust_version_from_cargo=see_evidence", std::env::consts::ARCH);
    for case in &workloads() {
        benchmark_case(case);
    }
    benchmark_batch();
    println!("identity_assertions=PASS canonical_wire_assertions=PASS reload_assertions=PASS");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_workload_has_identical_states_and_wire() {
        for case in workloads() {
            exercise(&case, true);
        }
    }

    #[test]
    fn identical_384_section_reload_and_wire() {
        let biomes = batch_biomes();
        for cells in batch_cells() {
            let base = production(&cells);
            let trial = PrototypeSection::from_cells(&cells);
            assert_equal("batch reload", &cells, &base, &trial, &biomes);
        }
    }

    #[test]
    fn all_air_types_and_boundary_repack() {
        for air in [0, 12958, 12959] {
            let cells = [air; CELLS];
            let base = production(&cells);
            let trial = PrototypeSection::from_cells(&cells);
            assert_eq!(base.palette_bits(), 0);
            assert_eq!(trial.bits(), 0);
            assert_equal("air", &cells, &base, &trial, &[3; 64]);
        }
        for (distinct, bits) in [
            (1, 0),
            (2, 4),
            (16, 4),
            (17, 5),
            (32, 5),
            (33, 6),
            (64, 6),
            (65, 7),
            (128, 7),
            (129, 8),
            (256, 8),
            (257, 15),
            (512, 15),
        ] {
            let cells: [u32; CELLS] = std::array::from_fn(|cell| state(cell % distinct));
            let base = production(&cells);
            let trial = PrototypeSection::from_cells(&cells);
            assert_eq!(base.palette_bits(), bits, "distinct={distinct}");
            assert_eq!(trial.bits(), bits, "distinct={distinct}");
            assert_equal("boundary", &cells, &base, &trial, &[3; 64]);
        }
    }
}
