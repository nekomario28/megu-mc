// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane R1 (H16): pin the multi-word light engine end to end on 16-, 24-, 64-
//! and 128-payload-section windows. Covers skylight and blocklight
//! initialization, local block-edit relighting at the section-index
//! boundaries 0, 63, 64 and 127, `BitSet.toLongArray`-shaped wire mask
//! encode/decode across three words, and fail-closed rejection of oversized
//! or malformed input without panic or unbounded allocation.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use mc_spine_kernel::{Error, LightEdit, LightEngine, LightFact, LightKind, LightMasksMultiWord};

struct CountingAlloc;

thread_local! {
    static ALLOC_CALLS: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_CALLS.with(|count| count.set(count.get() + 1));
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOC_CALLS.with(|count| count.set(count.get() + 1));
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_CALLS.with(|count| count.set(count.get() + 1));
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAlloc = CountingAlloc;

fn alloc_calls() -> usize {
    ALLOC_CALLS.with(Cell::get)
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

fn light_edit(x: i32, y: i32, z: i32, fact: LightFact) -> LightEdit {
    LightEdit { x, y, z, fact }
}

fn nibble(section: &[u8], x: usize, y: usize, z: usize) -> u8 {
    let cell = y << 8 | z << 4 | x;
    (section[cell / 2] >> (cell % 2 * 4)) & 0xf
}

/// `BitSet.toLongArray` decode: bit `i` is bit `i % 64` of word `i / 64`.
fn mask_indices(words: &[u64]) -> Vec<usize> {
    let mut indices = Vec::new();
    for (word_index, &word) in words.iter().enumerate() {
        let mut bits = word;
        while bits != 0 {
            indices.push(word_index * 64 + bits.trailing_zeros() as usize);
            bits &= bits - 1;
        }
    }
    indices
}

/// Every mask the kernel emits must stay `toLongArray`-shaped: a trailing
/// all-zero word is never produced, and a 130-section window never needs
/// more than three words.
fn assert_wire_shape(words: &[u64], section_count: usize) {
    assert!(
        words.len() <= section_count.div_ceil(64),
        "mask {words:?} exceeds the {section_count}-section word bound"
    );
    assert!(
        words.last().is_none_or(|word| *word != 0),
        "mask {words:?} keeps an untrimmed zero tail"
    );
}

/// The lit and empty masks of one light layer partition the whole window:
/// every section index is set in exactly one of them.
fn assert_mask_partition(lit: &[u64], empty: &[u64], section_count: usize) {
    let lit = mask_indices(lit);
    let empty = mask_indices(empty);
    assert!(
        lit.iter().all(|index| !empty.contains(index)),
        "section index set in both the lit and empty masks"
    );
    let mut union = lit;
    union.extend(empty);
    union.sort_unstable();
    assert_eq!(
        union,
        (0..section_count).collect::<Vec<_>>(),
        "lit/empty masks do not partition {section_count} sections"
    );
}

fn assert_init_wire_shape(masks: &LightMasksMultiWord, section_count: usize) {
    for words in [
        &masks.sky_mask,
        &masks.block_mask,
        &masks.empty_sky_mask,
        &masks.empty_block_mask,
    ] {
        assert_wire_shape(words, section_count);
    }
}

/// 16-section (Nether/End-style, no skylight), 24-section (Overworld
/// 1.21.1), 64- and 128-payload-section windows initialize both light
/// layers and split them into packet-shaped masks. The >62-payload windows
/// run the tall engine, where the legacy `u64` edit API must fail closed.
#[test]
fn windows_of_16_24_64_128_sections_initialize_sky_and_block_light() {
    for (min_y, height, has_skylight) in [
        (0, 256, false),    // 16 payload sections, Nether/End-like
        (-64, 384, true),   // 24 payload sections, Overworld 1.21.1
        (0, 1024, true),    // 64 payload sections
        (-512, 2048, true), // 128 payload sections, y -512..1536
    ] {
        let payload = (height / 16) as usize;
        let count = payload + 2;
        let mut engine = LightEngine::new_tall(min_y, height, has_skylight).unwrap();
        assert_eq!(engine.section_count(), count);
        assert_eq!(engine.cells_per_chunk(), height as usize * 256);

        // One transparent emitting cell mid-window proves blocklight
        // initialization without disturbing skylight.
        let emitter_y = min_y + (height / 2) as i32;
        let mut cells = column(height as usize, |_, _, _, _| AIR);
        cells[cell_index(min_y, 8, emitter_y, 8)] = LANTERN;
        engine.load_chunk(0, 0, &cells).unwrap();

        let masks = engine.section_masks_multi_word(0, 0).unwrap();
        assert_init_wire_shape(&masks, count);

        if has_skylight {
            // Every payload section plus the top guard is full-bright air;
            // only the below guard decodes from the empty mask.
            assert_eq!(
                mask_indices(&masks.sky_mask),
                (1..count).collect::<Vec<_>>()
            );
            assert_eq!(mask_indices(&masks.empty_sky_mask), vec![0]);
            assert_eq!(masks.sky_arrays.len(), count - 1);
            assert!(masks
                .sky_arrays
                .iter()
                .all(|array| array.iter().all(|&byte| byte == 0xff)));
        } else {
            assert!(masks.sky_mask.is_empty());
            assert!(masks.empty_sky_mask.is_empty());
            assert!(masks.sky_arrays.is_empty());
        }

        // Blocklight: only the emitter's 14-step spread is lit, and the lit
        // and empty masks partition the window exactly.
        let emitter_payload = (emitter_y - min_y) as usize / 16;
        let first = ((emitter_y - 14 - min_y).max(0) as usize) / 16;
        let last = ((emitter_y + 14 - min_y).min(height as i32 - 1) as usize) / 16;
        let expected: Vec<usize> = (first..=last).map(|section| section + 1).collect();
        assert_eq!(mask_indices(&masks.block_mask), expected);
        assert_eq!(masks.block_arrays.len(), expected.len());
        assert_mask_partition(&masks.block_mask, &masks.empty_block_mask, count);

        // The emitter cell reads its full emission through `section`.
        let packed = engine
            .section(0, 0, LightKind::Block, emitter_payload + 1)
            .unwrap();
        assert_eq!(nibble(&packed, 8, (emitter_y - min_y) as usize % 16, 8), 15);

        // Both edit APIs work where the u64 masks can represent the window;
        // the tall engine rejects the lossy one instead of truncating bits.
        let probe = [light_edit(8, min_y + 8, 8, AIR)];
        if payload > 62 {
            assert_eq!(
                engine.apply_edits(&probe).err(),
                Some(Error::OutOfRange)
            );
        }
        assert!(engine.apply_edits_multi_word(&probe).is_ok());
    }
}

/// Relighting at payload section indices 0, 63, 64 and 127 of a 128-section
/// window crosses both word seams: bits 63/64 sit at the word-0/word-1
/// boundary and bit 128 needs word 2.
#[test]
fn tall_128_local_edits_relight_section_indices_0_63_64_127() {
    let (min_y, height) = (-512, 2048u32);
    let mut engine = LightEngine::new_tall(min_y, height, true).unwrap();
    let air = column(2048, |_, _, _, _| AIR);
    engine.load_chunk(0, 0, &air).unwrap();
    engine.load_chunk(1, 0, &air).unwrap();

    // The u64 edit API cannot represent bits past 63 and fails closed.
    assert_eq!(
        engine
            .apply_edits(&[light_edit(8, min_y + 8, 8, LANTERN)])
            .err(),
        Some(Error::OutOfRange)
    );

    for payload in [0usize, 63, 64, 127] {
        let y = min_y + payload as i32 * 16 + 8;
        let update = engine
            .apply_edits_multi_word(&[light_edit(8, y, 8, LANTERN)])
            .unwrap();
        assert!(update.invalidated_cells > 0);
        let chunk = update
            .chunks
            .iter()
            .find(|chunk| chunk.chunk == (0, 0))
            .unwrap();
        // Emission 15 spreads 14 Manhattan steps: the emitter's section plus
        // one above and below, clamped to the window.
        let expected: Vec<usize> = (payload.saturating_sub(1)..=(payload + 1).min(127))
            .map(|section| section + 1)
            .collect();
        assert_eq!(mask_indices(&chunk.block_mask), expected);
        for words in [
            &chunk.sky_mask,
            &chunk.block_mask,
            &chunk.empty_sky_mask,
            &chunk.empty_block_mask,
        ] {
            assert_wire_shape(words, engine.section_count());
        }
        assert_eq!(
            chunk
                .block_sections
                .iter()
                .map(|(index, _)| *index)
                .collect::<Vec<_>>(),
            expected
        );
        let (_, array) = chunk
            .block_sections
            .iter()
            .find(|(index, _)| *index == payload + 1)
            .unwrap();
        assert_eq!(nibble(array, 8, 8, 8), 15);
        // Emission-only edits leave the sky layer untouched.
        assert!(chunk.sky_mask.is_empty() && chunk.sky_sections.is_empty());
        assert!(chunk.empty_sky_mask.is_empty());

        // Clearing the emitter darkens the same sections via the empty mask.
        let cleared = engine
            .apply_edits_multi_word(&[light_edit(8, y, 8, AIR)])
            .unwrap();
        let chunk = cleared
            .chunks
            .iter()
            .find(|chunk| chunk.chunk == (0, 0))
            .unwrap();
        assert_eq!(mask_indices(&chunk.empty_block_mask), expected);
        assert!(chunk.block_mask.is_empty() && chunk.block_sections.is_empty());
    }

    // An opaque edit at payload section 64 also relights skylight below it:
    // payload sections 0..=64 change -> bits 1..=65 in two words.
    let y = min_y + 64 * 16 + 8;
    let update = engine
        .apply_edits_multi_word(&[light_edit(8, y, 8, STONE)])
        .unwrap();
    let chunk = update
        .chunks
        .iter()
        .find(|chunk| chunk.chunk == (0, 0))
        .unwrap();
    assert_eq!(chunk.sky_mask, vec![0xffff_ffff_ffff_fffe, 0b11]);
    assert_eq!(mask_indices(&chunk.sky_mask), (1..=65).collect::<Vec<_>>());
    assert_eq!(chunk.sky_sections.len(), 65);
    // The occluding cell is dark; below the shadow column skylight leaks
    // back laterally to 14, one step from a lit column, as vanilla resolves.
    let occluded = engine.section(0, 0, LightKind::Sky, 65).unwrap();
    assert_eq!(nibble(&occluded, 8, 8, 8), 0);
    assert_eq!(
        nibble(&engine.section(0, 0, LightKind::Sky, 2).unwrap(), 8, 0, 8),
        14
    );
}

/// The four packet masks a 128-section column emits stay inside three u64
/// words, decode back to the exact section sets, and match the legacy u64
/// masks bit-for-bit on a window the standard engine still handles.
#[test]
fn multi_word_wire_masks_encode_and_decode_across_words() {
    let (min_y, height) = (-512, 2048u32);
    let mut engine = LightEngine::new_tall(min_y, height, true).unwrap();
    engine
        .load_chunk(0, 0, &column(2048, |_, _, _, _| AIR))
        .unwrap();

    let masks = engine.section_masks_multi_word(0, 0).unwrap();
    assert_init_wire_shape(&masks, 130);
    // Open air: payload sections 1..=128 plus the top guard (index 129) are
    // lit, so bits 1..=129 spread over three words; the below guard is dark.
    assert_eq!(
        masks.sky_mask,
        vec![0xffff_ffff_ffff_fffe, u64::MAX, 0b11]
    );
    assert_eq!(masks.empty_sky_mask, vec![1]);
    assert_eq!(masks.sky_arrays.len(), 129);
    assert!(masks
        .sky_arrays
        .iter()
        .all(|array| array.iter().all(|&byte| byte == 0xff)));
    // Every blocklight section is dark: no arrays, empty mask covers all
    // 130 indices.
    assert!(masks.block_mask.is_empty() && masks.block_arrays.is_empty());
    assert_eq!(masks.empty_block_mask, vec![u64::MAX, u64::MAX, 0b11]);
    // Decode round-trip: word expansion reproduces the exact section sets.
    assert_eq!(
        mask_indices(&masks.sky_mask),
        (1..130).collect::<Vec<_>>()
    );
    assert_eq!(
        mask_indices(&masks.empty_block_mask),
        (0..130).collect::<Vec<_>>()
    );
    assert_mask_partition(&masks.sky_mask, &masks.empty_sky_mask, 130);
    assert_mask_partition(&masks.block_mask, &masks.empty_block_mask, 130);

    // An edit past bit 63: payload 100 -> sections 99..=101 -> bits
    // 100..=102 -> word 1, bits 36..=38. Decoding the emitted mask must
    // return exactly the shipped section indices.
    let update = engine
        .apply_edits_multi_word(&[light_edit(8, min_y + 100 * 16 + 8, 8, LANTERN)])
        .unwrap();
    let chunk = update
        .chunks
        .iter()
        .find(|chunk| chunk.chunk == (0, 0))
        .unwrap();
    assert_eq!(chunk.block_mask, vec![0, 0x70_0000_0000]);
    assert_eq!(
        mask_indices(&chunk.block_mask),
        chunk
            .block_sections
            .iter()
            .map(|(index, _)| *index)
            .collect::<Vec<_>>()
    );

    // On a <=62-payload-section window the multi-word report keeps word
    // zero identical to the legacy u64 masks: same edits, same bits.
    let mut legacy = LightEngine::new(-64, 384, true).unwrap();
    let mut tall_api = LightEngine::new_tall(-64, 384, true).unwrap();
    let cells = column(384, |_, _, _, _| AIR);
    legacy.load_chunk(0, 0, &cells).unwrap();
    tall_api.load_chunk(0, 0, &cells).unwrap();
    let edit = [light_edit(8, 20, 8, TORCH)];
    let u64_update = legacy.apply_edits(&edit).unwrap();
    let words_update = tall_api.apply_edits_multi_word(&edit).unwrap();
    assert_eq!(
        u64_update.invalidated_cells,
        words_update.invalidated_cells
    );
    let single = |mask: u64| {
        if mask == 0 {
            Vec::new()
        } else {
            vec![mask]
        }
    };
    for (u64_chunk, words_chunk) in u64_update.chunks.iter().zip(&words_update.chunks) {
        assert_eq!(u64_chunk.chunk, words_chunk.chunk);
        assert_eq!(single(u64_chunk.sky_mask), words_chunk.sky_mask);
        assert_eq!(single(u64_chunk.empty_sky_mask), words_chunk.empty_sky_mask);
        assert_eq!(single(u64_chunk.block_mask), words_chunk.block_mask);
        assert_eq!(
            single(u64_chunk.empty_block_mask),
            words_chunk.empty_block_mask
        );
        assert_eq!(u64_chunk.sky_sections, words_chunk.sky_sections);
        assert_eq!(u64_chunk.block_sections, words_chunk.block_sections);
    }
}

/// Hostile geometry, malformed edits and short loads all fail closed:
/// rejection happens before any window-sized allocation, `catch_unwind`
/// proves the 129-section cap errors rather than panicking, and the u64
/// edit API refuses a window whose masks it cannot represent.
#[test]
fn oversized_and_malformed_inputs_fail_closed_without_allocation() {
    let mark = alloc_calls();
    let attempts: [fn() -> Result<(), Error>; 5] = [
        || LightEngine::new_tall(0, 16 * 129, true).map(|_| ()),
        || LightEngine::new_tall(0, 16 * 200, false).map(|_| ()),
        || LightEngine::new_tall(0, u32::MAX, true).map(|_| ()),
        || LightEngine::new_tall(3, 16, true).map(|_| ()),
        || LightEngine::new_tall(2_147_483_632, 16, true).map(|_| ()),
    ];
    for attempt in attempts {
        let result = std::panic::catch_unwind(attempt);
        assert!(
            matches!(result, Ok(Err(_))),
            "oversized geometry must error, not panic"
        );
    }
    assert_eq!(
        alloc_calls(),
        mark,
        "rejected geometry still allocated a window"
    );
    // The u64-only constructor keeps its pinned cap at 62 payload sections.
    assert!(LightEngine::new(0, 16 * 63, true).is_err());

    let mut engine = LightEngine::new_tall(-512, 2048, true).unwrap();
    // A short cell list is rejected before the chunk buffer is allocated.
    let mark = alloc_calls();
    assert_eq!(
        engine.load_chunk(0, 0, &[AIR; 16]).err(),
        Some(Error::OutOfRange)
    );
    assert_eq!(alloc_calls(), mark, "rejected load allocated memory");
    // Never-loaded columns report no sections and no masks.
    assert_eq!(
        engine.section(0, 0, LightKind::Sky, 0).err(),
        Some(Error::OutOfRange)
    );
    assert!(engine.section_masks_multi_word(0, 0).is_none());

    engine
        .load_chunk(0, 0, &column(2048, |_, _, _, _| AIR))
        .unwrap();

    // Section reads at and past the 130-section bound fail closed.
    assert_eq!(
        engine.section(0, 0, LightKind::Sky, 130).err(),
        Some(Error::OutOfRange)
    );
    assert_eq!(
        engine.section(0, 0, LightKind::Sky, usize::MAX).err(),
        Some(Error::OutOfRange)
    );

    // Malformed and out-of-window edits fail atomically: one bad edit voids
    // the whole batch and mutates nothing.
    let good = light_edit(8, 8, 8, TORCH);
    assert_eq!(
        engine.apply_edits_multi_word(&[]).err(),
        Some(Error::InvalidArgument)
    );
    for bad in [
        light_edit(8, 1536, 8, LANTERN),   // at the top bound (top = 1536)
        light_edit(8, -513, 8, LANTERN),   // below the window
        light_edit(24, 0, 8, LANTERN),     // chunk (1,0) never loaded
        light_edit(i32::MAX - 16, 0, 8, LANTERN), // absurd coords stay a clean error
        LightEdit {
            fact: LightFact {
                opacity: 16,
                ..LANTERN
            },
            ..good
        },
        LightEdit {
            fact: LightFact {
                emission: 16,
                ..LANTERN
            },
            ..good
        },
        LightEdit {
            fact: LightFact {
                occlusion: [0x10; 6],
                ..LANTERN
            },
            ..good
        },
    ] {
        assert!(engine.apply_edits_multi_word(&[bad]).is_err());
        assert!(engine.apply_edits_multi_word(&[good, bad]).is_err());
    }
    // Every rejected batch left the window dark.
    for index in 0..engine.section_count() {
        assert!(engine
            .section(0, 0, LightKind::Block, index)
            .unwrap()
            .iter()
            .all(|&byte| byte == 0));
    }
}
