//! Anti-xray and anti-fullbright against the generated world: what reaches the wire.
use super::*;

const SIDES: [(i32, i32, i32); 6] = [
    (1, 0, 0),
    (-1, 0, 0),
    (0, 1, 0),
    (0, -1, 0),
    (0, 0, 1),
    (0, 0, -1),
];

fn rv(b: &[u8], i: &mut usize) -> i64 {
    let (mut r, mut s) = (0i64, 0);
    loop {
        let x = b[*i];
        *i += 1;
        r |= ((x & 0x7f) as i64) << s;
        if x & 0x80 == 0 {
            return r;
        }
        s += 7;
    }
}

fn read_container(b: &[u8], i: &mut usize, n: usize, global: bool) -> Vec<u32> {
    let bits = b[*i] as usize;
    *i += 1;
    if bits == 0 {
        let v = rv(b, i) as u32;
        let len = rv(b, i) as usize;
        *i += len * 8;
        return vec![v; n];
    }
    let direct = (global && bits > 8) || (!global && bits > 3);
    let mut pal = Vec::new();
    if !direct {
        for _ in 0..rv(b, i) {
            pal.push(rv(b, i) as u32);
        }
    }
    let len = rv(b, i) as usize;
    let longs: Vec<u64> = (0..len)
        .map(|k| u64::from_be_bytes(b[*i + k * 8..*i + k * 8 + 8].try_into().unwrap()))
        .collect();
    *i += len * 8;
    let (per, mask) = (64 / bits, (1u64 << bits) - 1);
    (0..n)
        .map(|k| {
            let v = ((longs[k / per] >> ((k % per) * bits)) & mask) as u32;
            if direct {
                v
            } else {
                pal[v as usize]
            }
        })
        .collect()
}

/// The block states a chunk's section data puts on the wire.
fn wire_states(dim: DimId, cx: i32, cz: i32, gen: &mut GenSources) -> Vec<Vec<u32>> {
    let data = chunk_data_opt(cx, cz, dim, gen, true);
    let mut i = 0;
    let out = (0..dim.sections())
        .map(|_| {
            i += 2;
            let states = read_container(&data, &mut i, 4096, true);
            read_container(&data, &mut i, 64, false);
            states
        })
        .collect();
    assert_eq!(i, data.len());
    out
}

fn hidden_name(st: u32) -> bool {
    let name = breg().unwrap().name_of(st);
    HIDDEN_BLOCK_NAMES.contains(&name.trim_start_matches("minecraft:"))
}

/// Every hidden block with an opaque full cube on all six sides is sent as something that
/// is not a hidden block, in the Overworld and the Nether; a visible one is sent as it is.
/// Before: about a third of buried Overworld ores (deepslate ores won the filler vote
/// against deepslate) and every Nether gold/quartz ore reached the wire.
#[test]
fn buried_valuables_never_reach_the_wire_and_visible_ones_do() {
    // Generator initialisation is process-global: run in a child like the other
    // generated-world fixtures.
    if crate::test_support::isolated_world_test() {
        return;
    }
    let _guard = WORLD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = crate::worldgen::lock_gen_for_tests();
    assert!(worldgen::init(0, crate::name_to_state));
    assert!(worldgen::init_dim(DimId::Nether, 0, crate::name_to_state));
    assert!(anti_xray_enabled());
    let reg = breg().unwrap();
    for (dim, chunks) in [
        (DimId::Overworld, &[(0, 0), (5, -3), (-7, 4)][..]),
        (DimId::Nether, &[(0, 0), (3, -2)][..]),
    ] {
        let mut gen: GenSources = HashMap::new();
        let (mut buried, mut visible) = (0, 0);
        for &(cx, cz) in chunks {
            let wire = wire_states(dim, cx, cz, &mut gen);
            for (s, states) in wire.iter().enumerate() {
                for (idx, &w) in states.iter().enumerate() {
                    let x = cx * 16 + (idx & 15) as i32;
                    let z = cz * 16 + ((idx >> 4) & 15) as i32;
                    let y = dim.min_y() + s as i32 * 16 + (idx >> 8) as i32;
                    let t = block_at_dim(dim, x, y, z);
                    if !hidden_name(t) {
                        assert!(
                            !hidden_name(w),
                            "{dim:?} {x},{y},{z}: invented a hidden block"
                        );
                        continue;
                    }
                    let covered = SIDES.iter().all(|(a, b, c)| {
                        reg.is_solid_render(block_at_dim(dim, x + a, y + b, z + c))
                    });
                    if covered {
                        buried += 1;
                        assert!(
                            !hidden_name(w),
                            "{dim:?} {x},{y},{z}: buried {} sent as {}",
                            reg.name_of(t),
                            reg.name_of(w)
                        );
                    } else if SIDES.iter().any(|(a, b, c)| {
                        !reg.is_solid_render(block_at_dim(dim, x + a, y + b, z + c))
                            && !hidden_name(block_at_dim(dim, x + a, y + b, z + c))
                    }) {
                        visible += 1;
                        assert_eq!(w, t, "{dim:?} {x},{y},{z}: a visible block was masked");
                    }
                }
            }
        }
        assert!(
            buried > 200 && visible > 0,
            "{dim:?}: {buried} buried, {visible} visible"
        );
    }

    // Nether edits: a buried debris is disguised as netherrack, revealed when a neighbour is
    // mined and hidden again when it is filled.
    let rack = name_to_state("netherrack").unwrap();
    let debris = name_to_state("ancient_debris").unwrap();
    let (x, y, z) = (40, 30, 40);
    for (a, b, c) in SIDES.iter().copied().chain([(0, 0, 0), (2, 0, 0)]) {
        set_block_dim(DimId::Nether, x + a, y + b, z + c, rack);
    }
    set_block_dim(DimId::Nether, x, y, z, debris);
    assert_eq!(
        visible_block_update_state(DimId::Nether, x, y, z, debris),
        rack
    );
    let key = (
        DimId::Nether,
        x.div_euclid(16),
        y.div_euclid(16),
        z.div_euclid(16),
    );
    let li = (((x & 15) << 8) | ((z & 15) << 4) | (y & 15)) as u16;
    let update = || {
        pending_blocks()
            .lock()
            .unwrap()
            .get(&key)
            .and_then(|m| m.get(&li).copied())
    };
    std::mem::take(&mut *pending_blocks().lock().unwrap());
    set_block_dim(DimId::Nether, x + 1, y, z, 0);
    assert_eq!(
        update(),
        Some(debris),
        "mining the neighbour reveals the debris"
    );
    std::mem::take(&mut *pending_blocks().lock().unwrap());
    set_block_dim(DimId::Nether, x + 1, y, z, rack);
    assert_eq!(update(), Some(rack), "filling it hides the debris again");
    std::mem::take(&mut *pending_blocks().lock().unwrap());
    for (a, b, c) in SIDES.iter().copied().chain([(0, 0, 0), (2, 0, 0)]) {
        let (px, py, pz) = (x + a, y + b, z + c);
        set_block_dim(
            DimId::Nether,
            px,
            py,
            pz,
            base_state_dim(DimId::Nether, px, py, pz),
        );
    }
    std::mem::take(&mut *pending_blocks().lock().unwrap());
}

/// Every state of a hidden block is hidden, and see-through blocks expose a face.
#[test]
fn hidden_set_covers_every_state_and_see_through_blocks_expose() {
    let reg = breg().unwrap();
    for name in [
        "redstone_ore",
        "chest",
        "barrel",
        "nether_gold_ore",
        "nether_quartz_ore",
    ] {
        let states: Vec<u32> = (0..reg.state_count() as u32)
            .filter(|&s| reg.name_of(s).trim_start_matches("minecraft:") == name)
            .collect();
        assert!(!states.is_empty(), "{name}");
        assert!(states.iter().all(|&s| is_hidden(s)), "{name}");
    }
    for name in [
        "air",
        "cave_air",
        "water",
        "glass",
        "tinted_glass",
        "oak_slab",
        "short_grass",
        "oak_leaves",
    ] {
        assert!(exposes(name_to_state(name).unwrap()), "{name}");
    }
    for name in ["stone", "deepslate", "netherrack", "dirt", "diamond_ore"] {
        assert!(!exposes(name_to_state(name).unwrap()), "{name}");
    }
}

fn nibble(arr: &[u8; 2048], k: usize) -> u8 {
    (arr[k >> 1] >> ((k & 1) * 4)) & 15
}

/// Decode the light half of a chunk packet back into full section lists.
fn read_light(p: &[u8], sections: usize) -> (Vec<[u8; 2048]>, Vec<[u8; 2048]>, usize) {
    let mut i = 0;
    let mut masks = [0u64; 4];
    for m in &mut masks {
        let n = rv(p, &mut i);
        assert!(n <= 1);
        if n == 1 {
            *m = u64::from_be_bytes(p[i..i + 8].try_into().unwrap());
            i += 8;
        }
    }
    let mut arrays = 0;
    let mut read = |mask: u64, empty: u64, i: &mut usize| {
        assert_eq!(mask & empty, 0);
        let n = rv(p, i) as u32;
        assert_eq!(n, mask.count_ones());
        let mut out = vec![[0u8; 2048]; sections];
        for (s, sec) in out.iter_mut().enumerate() {
            if mask & (1 << s) != 0 {
                assert_eq!(rv(p, i), 2048);
                sec.copy_from_slice(&p[*i..*i + 2048]);
                *i += 2048;
                arrays += 1;
            }
        }
        out
    };
    let sky = read(masks[0], masks[2], &mut i);
    let block = read(masks[1], masks[3], &mut i);
    assert_eq!(i, p.len());
    (sky, block, arrays)
}

/// All-zero light sections travel in the empty masks with no array, and nothing else
/// changes: the decoded light equals the computed light bit for bit.
#[test]
fn zero_light_sections_use_the_empty_masks_losslessly() {
    // Generator initialisation is process-global: run in a child like the other
    // generated-world fixtures.
    if crate::test_support::isolated_world_test() {
        return;
    }
    let _guard = WORLD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = crate::worldgen::lock_gen_for_tests();
    assert!(worldgen::init(0, crate::name_to_state));
    ac_test_set_anti_fullbright(Some(false));
    let mut gen: GenSources = HashMap::new();
    let n = DimId::Overworld.light_sections();
    let mut saved = 0;
    for (cx, cz) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
        chunk_data_opt(cx, cz, DimId::Overworld, &mut gen, true);
    }
    for (cx, cz) in [(0, 0), (1, 1)] {
        let mut p = Vec::new();
        write_light_arrays(&mut p, cx, cz, DimId::Overworld, &gen);
        let (sky, block, arrays) = read_light(&p, n);
        assert_eq!(sky, sky_light_sections_dim(DimId::Overworld, cx, cz));
        assert_eq!(
            block,
            block_light_sections_dim(DimId::Overworld, cx, cz, &gen)
        );
        assert!(arrays < 2 * n, "all-zero sections were sent as arrays");
        saved += (2 * n - arrays) * 2048;
    }
    assert!(saved > 2 * 20 * 2048, "saved {saved} bytes");
    ac_test_set_anti_fullbright(None);
}

/// antifullbright hides the block light of sealed generated pockets only: nothing it
/// clears is reachable by sky light, open caves keep their light, the switch off sends
/// everything, and opening a pocket brings its real light back.
#[test]
fn sealed_pocket_light_is_hidden_until_the_pocket_is_opened() {
    // Generator initialisation is process-global: run in a child like the other
    // generated-world fixtures.
    if crate::test_support::isolated_world_test() {
        return;
    }
    let _guard = WORLD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = crate::worldgen::lock_gen_for_tests();
    assert!(worldgen::init(0, crate::name_to_state));
    let dim = DimId::Overworld;
    let reg = breg().unwrap();
    let mut gen: GenSources = HashMap::new();
    let mut found = None;
    let (mut lit_total, mut hidden_total) = (0, 0);
    'search: for cx in 0..6 {
        for cz in 0..6 {
            for dx in -1..=1 {
                for dz in -1..=1 {
                    if !gen.contains_key(&(cx + dx, cz + dz)) {
                        chunk_data_opt(cx + dx, cz + dz, dim, &mut gen, true);
                    }
                }
            }
            let real = block_light_sections_dim(dim, cx, cz, &gen);
            let sky = sky_light_sections_dim(DimId::Overworld, cx, cz);
            ac_test_set_anti_fullbright(Some(false));
            let mut off = real.clone();
            assert_eq!(
                hide_sealed_block_light(dim, cx, cz, None, off.iter_mut().enumerate()),
                0
            );
            ac_test_set_anti_fullbright(Some(true));
            let mut on = real.clone();
            let cleared = hide_sealed_block_light(dim, cx, cz, None, on.iter_mut().enumerate());
            lit_total += real.iter().flatten().filter(|b| **b != 0).count();
            hidden_total += cleared;
            for (s, (r, o)) in real.iter().zip(&on).enumerate() {
                for k in 0..4096 {
                    if nibble(r, k) != 0 && nibble(o, k) == 0 {
                        let p = (
                            cx * 16 + (k & 15) as i32,
                            dim.light_min_y() + 16 * s as i32 + (k >> 8) as i32,
                            cz * 16 + ((k >> 4) & 15) as i32,
                        );
                        assert_eq!(nibble(&sky[s], k), 0, "{p:?}: sky reaches a hidden cell");
                        if found.is_none() && reg.opacity_of(block_at_dim(dim, p.0, p.1, p.2)) < 15
                        {
                            found = Some((cx, cz, p, real.clone()));
                        }
                    } else {
                        assert_eq!(nibble(r, k), nibble(o, k));
                    }
                }
            }
            if found.is_some() {
                break 'search;
            }
        }
    }
    let (cx, cz, p, real) = found.unwrap_or_else(|| {
        panic!("no sealed lit pocket in 36 chunks ({hidden_total}/{lit_total} lit bytes)")
    });
    // Open the pocket: mine the first opaque wall next to the hidden cell.
    let (wx, wy, wz) = SIDES
        .iter()
        .map(|(a, b, c)| (p.0 + a, p.1 + b, p.2 + c))
        .find(|&(x, y, z)| reg.opacity_of(block_at_dim(dim, x, y, z)) == 15)
        .expect("a pocket has a wall");
    let before = block_at_dim(dim, wx, wy, wz);
    light_reveals().lock().unwrap().clear();
    set_block(wx, wy, wz, 0);
    assert!(
        light_reveals()
            .lock()
            .unwrap()
            .get(&dim)
            .is_some_and(|v| v.contains(&Position::new(wx, wy, wz))),
        "opening the pocket queues a light re-send"
    );
    let mut on = real.clone();
    hide_sealed_block_light(dim, cx, cz, None, on.iter_mut().enumerate());
    let s = ((p.1 - dim.light_min_y()) / 16) as usize;
    let k = (((p.1 - dim.light_min_y()) & 15) as usize) << 8
        | (((p.2 - cz * 16) as usize) << 4)
        | (p.0 - cx * 16) as usize;
    assert_eq!(
        nibble(&on[s], k),
        nibble(&real[s], k),
        "opened pocket keeps its light"
    );
    set_block(wx, wy, wz, before);
    light_reveals().lock().unwrap().clear();
    ac_test_set_anti_fullbright(None);
    println!("antifullbright: {hidden_total} of {lit_total} lit light bytes cleared before the first pocket");
}

/// Buried live block entities (suspicious sand/gravel LootTableSeed, vaults, etc.)
/// must not leak their block-entity NBT in `0x27` chunk frames while anti-xray is on,
/// and must appear immediately once an adjacent block is exposed.
#[test]
fn buried_block_entity_nbt_is_suppressed_until_exposed() {
    if crate::test_support::isolated_world_test() {
        return;
    }
    let _guard = WORLD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = crate::worldgen::lock_gen_for_tests();
    assert!(worldgen::init(0, crate::name_to_state));
    assert!(anti_xray_enabled());

    let stone = name_to_state("stone").unwrap();
    let susp = name_to_state("suspicious_sand").unwrap();
    let (x, y, z) = (24, 30, 24);
    let saved: Vec<((i32, i32, i32), u32)> = SIDES
        .iter()
        .copied()
        .chain([(0, 0, 0)])
        .map(|(dx, dy, dz)| {
            let p = (x + dx, y + dy, z + dz);
            (p, block_at(p.0, p.1, p.2))
        })
        .collect();
    for (dx, dy, dz) in SIDES {
        set_block(x + dx, y + dy, z + dz, stone);
    }
    set_block(x, y, z, susp);
    live_block_changed(DimId::Overworld, RsPos(x, y, z), stone, susp);

    // Buried: live_chunk_block_entities has the entry, but cached_chunk_frame filters it out.
    let mut gen: GenSources = HashMap::new();
    let buried_frame = cached_chunk_frame(None, &mut gen, DimId::Overworld, x >> 4, z >> 4, false);
    // Expose one face and rebuild.
    set_block(x + 1, y, z, 0);
    let exposed_frame = cached_chunk_frame(None, &mut gen, DimId::Overworld, x >> 4, z >> 4, false);
    assert!(
        exposed_frame.len() > buried_frame.len()
            || buried_frame != exposed_frame,
        "exposing buried block entity must emit its NBT into the chunk frame"
    );
    assert!(!block_exposed_dim(DimId::Overworld, x, y, z) || block_at(x + 1, y, z) == 0);

    // Clean up restored blocks.
    for ((px, py, pz), st) in saved {
        set_block(px, py, pz, st);
    }
}

/// Conventional mod ore/chest tags (`#c:ores`, `#forge:ores`, `#c:chests`, plus
/// recursive sub-tags like `#c:ores/custom`) automatically add every matching block
/// state into `hidden_extra()` without manual ID configuration.
#[test]
fn conventional_c_and_forge_ore_tags_auto_register_for_antixray() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/tmp")
        .join(format!("ac-tag-ores-{}", std::process::id()));
    let c_ores_dir = dir.join("data/c/tags/block/ores");
    let forge_dir = dir.join("data/forge/tags/blocks");
    std::fs::create_dir_all(&c_ores_dir).unwrap();
    std::fs::create_dir_all(&forge_dir).unwrap();
    // Recursive tag: #c:ores -> #c:ores/custom -> minecraft:raw_gold_block
    // Legacy Forge tag: #forge:ores -> minecraft:raw_iron_block
    std::fs::write(
        dir.join("data/c/tags/block/ores.json"),
        r##"{"values": ["#c:ores/custom"]}"##,
    )
    .unwrap();
    std::fs::write(
        c_ores_dir.join("custom.json"),
        r#"{"values": ["minecraft:raw_gold_block"]}"#,
    )
    .unwrap();
    std::fs::write(
        forge_dir.join("ores.json"),
        r#"{"values": [{"id": "minecraft:raw_iron_block", "required": false}]}"#,
    )
    .unwrap();

    let gold = name_to_state("raw_gold_block").unwrap();
    let iron = name_to_state("raw_iron_block").unwrap();
    assert!(!hidden_states().contains(&gold));
    assert!(!hidden_states().contains(&iron));

    let added = register_hidden_tags_from_dir(&dir);
    assert!(added >= 2, "expected >=2 states auto-registered, got {added}");
    assert!(is_hidden(gold), "#c:ores sub-tag target must be auto-hidden");
    assert!(is_hidden(iron), "#forge:ores target must be auto-hidden");

    // Clean up test state and directory.
    {
        let mut extra = hidden_extra().lock().unwrap();
        extra.remove(&gold);
        extra.remove(&iron);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 2-lane underground occlusion: the surface player's `occluded` chunk lane.
// ---------------------------------------------------------------------------

/// The (block count, states) each section of a chunk encode puts on the wire,
/// for one anti-xray lane.
fn wire_sections_lane(
    dim: DimId,
    cx: i32,
    cz: i32,
    gen: &mut GenSources,
    occluded: bool,
) -> Vec<(u16, Vec<u32>)> {
    let data = chunk_data_lane(cx, cz, dim, gen, true, occluded);
    let mut i = 0;
    let out = (0..dim.sections())
        .map(|_| {
            let count = u16::from_be_bytes(data[i..i + 2].try_into().unwrap());
            i += 2;
            let states = read_container(&data, &mut i, 4096, true);
            read_container(&data, &mut i, 64, false);
            (count, states)
        })
        .collect();
    assert_eq!(i, data.len());
    out
}

/// Skip one NBT payload (the tag id is already known).
fn skip_nbt(b: &[u8], i: &mut usize, id: u8) {
    match id {
        0 => {}
        1 => *i += 1,
        2 => *i += 2,
        3 | 5 => *i += 4,
        4 | 6 => *i += 8,
        7 => {
            let n = i32::from_be_bytes(b[*i..*i + 4].try_into().unwrap());
            *i += 4 + n as usize;
        }
        8 => {
            let n = u16::from_be_bytes(b[*i..*i + 2].try_into().unwrap());
            *i += 2 + n as usize;
        }
        9 => {
            let el = b[*i];
            *i += 1;
            let n = i32::from_be_bytes(b[*i..*i + 4].try_into().unwrap());
            *i += 4;
            for _ in 0..n {
                skip_nbt(b, i, el);
            }
        }
        10 => loop {
            let t = b[*i];
            *i += 1;
            if t == 0 {
                break;
            }
            let n = u16::from_be_bytes(b[*i..*i + 2].try_into().unwrap()) as usize;
            *i += 2 + n;
            skip_nbt(b, i, t);
        },
        11 => {
            let n = i32::from_be_bytes(b[*i..*i + 4].try_into().unwrap());
            *i += 4 + n as usize * 4;
        }
        12 => {
            let n = i32::from_be_bytes(b[*i..*i + 4].try_into().unwrap());
            *i += 4 + n as usize * 8;
        }
        _ => panic!("unknown NBT tag {id}"),
    }
}

/// The index just past a `map_chunk` payload's block entities, after parsing
/// them into (local x, y, local z).
fn map_chunk_block_entities(payload: &[u8]) -> Vec<(i32, i32, i32)> {
    let mut i = 8; // chunk x and z
    assert_eq!(payload[i], 10, "heightmaps compound");
    i += 1;
    skip_nbt(payload, &mut i, 10);
    let data_len = rv(payload, &mut i) as usize;
    i += data_len;
    let n = rv(payload, &mut i);
    let mut out = Vec::new();
    for _ in 0..n {
        let xz = payload[i];
        i += 1;
        let y = i16::from_be_bytes(payload[i..i + 2].try_into().unwrap()) as i32;
        i += 2;
        rv(payload, &mut i); // block entity type
        assert_eq!(payload[i], 10, "block entity NBT root");
        i += 1;
        skip_nbt(payload, &mut i, 10);
        out.push(((xz >> 4) as i32, y, (xz & 15) as i32));
    }
    out
}

/// The block entities inside a whole `0x27` wire frame.
fn frame_block_entities(frame: &[u8]) -> Vec<(i32, i32, i32)> {
    let mut i = 0;
    let len = rv(frame, &mut i) as usize;
    assert_eq!(len, frame.len() - i, "frame length prefix");
    assert_eq!(rv(frame, &mut i), 0x27);
    map_chunk_block_entities(&frame[i..])
}

/// The decoded sky and block light arrays of a `map_chunk` payload.
fn map_chunk_light(payload: &[u8], dim: DimId) -> (Vec<[u8; 2048]>, Vec<[u8; 2048]>) {
    let mut i = 8;
    assert_eq!(payload[i], 10, "heightmaps compound");
    i += 1;
    skip_nbt(payload, &mut i, 10);
    let data_len = rv(payload, &mut i) as usize;
    i += data_len;
    let n = rv(payload, &mut i);
    for _ in 0..n {
        i += 3; // packed x/z and the y short
        rv(payload, &mut i);
        assert_eq!(payload[i], 10, "block entity NBT root");
        i += 1;
        skip_nbt(payload, &mut i, 10);
    }
    let (sky, block, _) = read_light(&payload[i..], dim.light_sections());
    (sky, block)
}

/// The surface player's occluded lane: every section whose top is at or below
/// `occlude_ceil_y` is a single-palette solid host rock section (deepslate under
/// Y=0, stone above) with block_count = 4096 — caves, mineshafts, stronghold
/// blocks, ores, cave air and buried bases all disappear — while every section
/// above the ceiling is byte-identical to the real lane. Underground block
/// entities and cave glow are stripped just as hard.
#[test]
fn underground_occlusion_masks_caves_and_stronghold_sections_for_surface_player() {
    // Generator initialisation is process-global: run in a child like the other
    // generated-world fixtures.
    if crate::test_support::isolated_world_test() {
        return;
    }
    let _guard = WORLD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = crate::worldgen::lock_gen_for_tests();
    assert!(worldgen::init(0, crate::name_to_state));
    assert!(anti_xray_enabled());
    ac_test_set_underground_occlusion(Some(true));

    let dim = DimId::Overworld;
    let (mut cx, mut cz, mut ceil) = (0, 0, None);
    for c in [(0, 0), (1, 0), (0, 1), (-1, 0), (0, -1), (4, 4)] {
        if let Some(y) = underground_occlude_ceil(dim, c.0, c.1) {
            if y > dim.min_y() {
                (cx, cz, ceil) = (c.0, c.1, Some(y));
                break;
            }
        }
    }
    let ceil = ceil.expect("an overworld chunk always has masked depth");
    // The ceiling really is the chunk's `hmin` rule, not a constant.
    let mut hmin = i32::MAX;
    for lz in 0..16 {
        for lx in 0..16 {
            hmin = hmin.min(height_at_dim(dim, cx * 16 + lx, cz * 16 + lz));
        }
    }
    assert_eq!(ceil, occlusion_ceil_from_hmin(hmin));
    assert!(ceil <= 32 && ceil > dim.min_y(), "ceil {ceil}");

    // Plant what a surface x-rayer wants most: a stronghold frame, an ore, an
    // open cave pocket and a block entity — all below the ceiling. An air cell
    // above the block entity leaves it "exposed", so the real lane carries it
    // and only the occluded lane's y-cut can strip it.
    let frame = name_to_state("end_portal_frame").unwrap();
    let ore = name_to_state("diamond_ore").unwrap();
    let susp = name_to_state("suspicious_sand").unwrap();
    let (px, pz) = (cx * 16 + 4, cz * 16 + 4);
    let py = ceil - 8;
    let edited: Vec<(i32, i32, i32)> = [
        (px, py, pz),
        (px + 2, py, pz),
        (px + 4, py, pz),
        (px + 5, py, pz),
        (px + 4, py + 1, pz),
        (px + 8, py, pz),
        (px + 8, py + 1, pz),
    ]
    .into_iter()
    .collect();
    let saved: Vec<u32> = edited
        .iter()
        .map(|&(x, y, z)| block_at_dim(dim, x, y, z))
        .collect();
    let susp_old = saved[5];
    set_block(px, py, pz, frame);
    set_block(px + 2, py, pz, ore);
    for (ax, ay, az) in [(4, 0, 0), (5, 0, 0), (4, 1, 0)] {
        set_block(px + ax, py + ay, pz + az, 0); // a pocket of cave air
    }
    set_block(px + 8, py, pz, susp);
    set_block(px + 8, py + 1, pz, 0);
    live_block_changed(dim, RsPos(px + 8, py, pz), susp_old, susp);

    let deepslate = name_to_state("deepslate").unwrap();
    let stone = name_to_state("stone").unwrap();
    let mut gen_occ: GenSources = HashMap::new();
    let occ = wire_sections_lane(dim, cx, cz, &mut gen_occ, true);
    let mut gen_rev: GenSources = HashMap::new();
    let rev = wire_sections_lane(dim, cx, cz, &mut gen_rev, false);
    let min_y = dim.min_y();
    let mut masked = 0;
    for (s, ((ocount, ostates), (_, rstates))) in occ.iter().zip(&rev).enumerate() {
        let sy = min_y + 16 * s as i32;
        if sy + 16 <= ceil {
            masked += 1;
            let host = if sy < 0 { deepslate } else { stone };
            assert_eq!(*ocount, 4096, "masked section {sy} block count");
            assert!(
                ostates.iter().all(|&st| st == host),
                "masked section {sy} is not uniformly host rock"
            );
            assert!(
                ostates.iter().all(|&st| !hidden_name(st)),
                "masked section {sy} still shows a hidden block"
            );
            continue;
        }
        assert_eq!(
            ostates, rstates,
            "section {sy} is above the ceiling: the lane must not touch it"
        );
    }
    assert!(masked > 0, "no section was masked");
    // The planted deep blocks are genuinely in the world…
    assert_eq!(block_at_dim(dim, px, py, pz), frame);
    assert_eq!(block_at_dim(dim, px + 2, py, pz), ore);
    // …but the occluded lane's wire at their cells is plain host rock.
    let s = ((py - min_y) / 16) as usize;
    let sy = min_y + 16 * s as i32;
    let host = if sy < 0 { deepslate } else { stone };
    for (ax, az) in [(4, 4), (6, 4), (8, 4), (12, 4)] {
        let idx = (((py - sy) as usize) << 8) | ((az as usize) << 4) | (ax & 15) as usize;
        assert_eq!(occ[s].1[idx], host, "deep cell (lx {ax}, lz {az}) leaked");
    }

    // Block entities under the ceiling — generated spawners, stronghold chests,
    // and the live suspicious sand planted above — never reach the wire.
    let mut gen: GenSources = HashMap::new();
    let occ_frame = cached_chunk_frame(None, &mut gen, dim, cx, cz, true);
    let occ_bes = frame_block_entities(&occ_frame);
    assert!(
        occ_bes.iter().all(|&(_, y, _)| y >= ceil),
        "occluded frame leaked a deep block entity: {occ_bes:?}"
    );
    let mut gen: GenSources = HashMap::new();
    let rev_frame = cached_chunk_frame(None, &mut gen, dim, cx, cz, false);
    let rev_bes = frame_block_entities(&rev_frame);
    assert!(
        rev_bes
            .iter()
            .any(|&(lx, y, lz)| lx == (px + 8) & 15 && y == py && lz == pz & 15),
        "the exposed live block entity belongs on the real lane: {rev_bes:?}"
    );

    // No light survives the mask either: every light band fully under the
    // ceiling ships zeroed in both arrays, whatever glows down there.
    let mut gen: GenSources = HashMap::new();
    let payload = map_chunk_lane(cx, cz, dim, &mut gen, true);
    let (sky, block) = map_chunk_light(&payload, dim);
    let lmin = dim.light_min_y();
    for (s, (sk, bl)) in sky.iter().zip(&block).enumerate() {
        if lmin + 16 * (s as i32 + 1) <= ceil {
            assert!(sk.iter().all(|&b| b == 0), "sky light leaked at band {s}");
            assert!(bl.iter().all(|&b| b == 0), "block light leaked at band {s}");
        }
    }

    // Restore the world and the override.
    for (&(x, y, z), st) in edited.iter().zip(saved) {
        set_block(x, y, z, st);
    }
    ac_test_set_underground_occlusion(None);
}

/// Lane hysteresis and the seamless reveal: `should_occlude_underground` keeps
/// a surface player occluded until they are under Y=48 or eight blocks under
/// the local surface, keeps a revealed player revealed through the band, and
/// re-occludes only clearly back on top. `update_underground_lane` turns a flip
/// into an immediate nearest-first re-send of everything the client holds.
#[test]
fn underground_occlusion_reveals_on_descent_and_hysteresis() {
    ac_test_set_underground_occlusion(Some(true));
    let ow = DimId::Overworld;
    // A surface player (still occluded) stays occluded…
    assert!(should_occlude_underground(ow, 70.0, 64, 0, false));
    assert!(should_occlude_underground(ow, 56.0, 64, 0, false));
    // …until they drop under Y=48 or more than 8 under the local surface.
    assert!(!should_occlude_underground(ow, 47.9, 64, 0, false));
    assert!(!should_occlude_underground(ow, 30.0, 64, 0, false));
    assert!(!should_occlude_underground(ow, 55.0, 64, 0, false)); // 55 < 64-8
    assert!(!should_occlude_underground(ow, 61.0, 70, 0, false)); // 61 < 70-8
    // A revealed player stays revealed through the whole hysteresis band…
    assert!(!should_occlude_underground(ow, 47.9, 64, 0, true));
    assert!(!should_occlude_underground(ow, 55.0, 64, 0, true));
    assert!(!should_occlude_underground(ow, 59.9, 64, 0, true)); // < 60
    assert!(!should_occlude_underground(ow, 61.0, 64, 0, true)); // 61 < 64-2
    // …and only re-occludes clearly back on the surface.
    assert!(should_occlude_underground(ow, 62.0, 64, 0, true));
    assert!(should_occlude_underground(ow, 70.0, 64, 0, true));
    assert!(should_occlude_underground(ow, 100.0, 96, 0, true));

    if crate::test_support::isolated_world_test() {
        ac_test_set_underground_occlusion(None);
        return;
    }
    let _guard = WORLD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = crate::worldgen::lock_gen_for_tests();
    assert!(worldgen::init(0, crate::name_to_state));

    // A client on the surface: the lane evaluates occluded, nothing is resent.
    let (client, mut receiver) = crate::effect_tests::client_pair();
    *client.pos.lock().unwrap() = (8.0, 70.0, 8.0, 0.0, 0.0, true);
    client.chunks.lock().unwrap().insert((0, 0));
    client.chunks.lock().unwrap().insert((3, 0));
    assert!(update_underground_lane(&client));
    assert!(!client.underground_revealed.load(Ordering::Relaxed));
    assert!(
        recv_wire_frame(&mut receiver).is_none(),
        "no flip, no resend"
    );

    // Descend below Y=48 inside the same column: the lane flips to revealed and
    // every held chunk is re-sent real, nearest first.
    client.pos.lock().unwrap().1 = 30.0;
    assert!(!update_underground_lane(&client));
    assert!(client.underground_revealed.load(Ordering::Relaxed));
    let mut gen: GenSources = HashMap::new();
    let near = cached_chunk_frame(None, &mut gen, DimId::Overworld, 0, 0, false);
    let far = cached_chunk_frame(None, &mut gen, DimId::Overworld, 3, 0, false);
    let f1 = recv_wire_frame(&mut receiver).expect("first resent frame");
    let f2 = recv_wire_frame(&mut receiver).expect("second resent frame");
    assert_eq!(f1, *near, "the player's own chunk reveals first");
    assert_eq!(f2, *far, "then the rest, nearest-first");
    // What arrives is the real chunk — caves and strongholds, not the mask.
    let mut gen: GenSources = HashMap::new();
    let masked = cached_chunk_frame(None, &mut gen, DimId::Overworld, 0, 0, true);
    assert_ne!(f1, *masked, "the reveal must be the real lane");
    assert!(
        recv_wire_frame(&mut receiver).is_none(),
        "exactly the held chunks are resent"
    );

    // Inside the hysteresis band the lane does not flap: back up to Y=55 stays
    // revealed, and only the clear-surface threshold re-occludes.
    client.pos.lock().unwrap().1 = 55.0;
    assert!(!update_underground_lane(&client));
    assert!(client.underground_revealed.load(Ordering::Relaxed));
    let surface = height_at_dim(DimId::Overworld, 8, 8);
    client.pos.lock().unwrap().1 = ((surface + 4).max(61)) as f64;
    assert!(update_underground_lane(&client));
    assert!(!client.underground_revealed.load(Ordering::Relaxed));
    let f1 = recv_wire_frame(&mut receiver).expect("re-occluded first frame");
    let f2 = recv_wire_frame(&mut receiver).expect("re-occluded second frame");
    let mut gen: GenSources = HashMap::new();
    let near = cached_chunk_frame(None, &mut gen, DimId::Overworld, 0, 0, true);
    let far = cached_chunk_frame(None, &mut gen, DimId::Overworld, 3, 0, true);
    assert_eq!(f1, *near, "resurfacing re-arms the occluded lane first");
    assert_eq!(f2, *far);
    ac_test_set_underground_occlusion(None);
}

/// Read one length-prefixed wire frame; `None` when nothing was sent.
fn recv_wire_frame(s: &mut std::net::TcpStream) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut raw = Vec::new();
    let mut len = 0i64;
    let mut shift = 0;
    loop {
        let mut b = [0u8; 1];
        match s.read(&mut b) {
            Ok(1) => {
                raw.push(b[0]);
                len |= ((b[0] & 0x7f) as i64) << shift;
                if b[0] & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            _ => return None,
        }
    }
    let mut body = vec![0u8; len as usize];
    s.read_exact(&mut body).ok()?;
    raw.extend_from_slice(&body);
    Some(raw)
}

/// Spectators, non-skylight dimensions, and both config off-switches all keep
/// the real lane — and even if an `occluded` flag reached the encode for the
/// Nether or End, it changes nothing there.
#[test]
fn underground_occlusion_skips_spectators_and_nether_end_and_config_off() {
    ac_test_set_underground_occlusion(Some(true));
    let ow = DimId::Overworld;
    // Spectator mode always reveals, whatever height it is at.
    assert!(!should_occlude_underground(ow, 70.0, 64, 3, false));
    assert!(!should_occlude_underground(ow, 30.0, 64, 3, false));
    assert!(!should_occlude_underground(ow, 70.0, 64, 3, true));
    // Nether and End have no skylight: open cavities are normal there.
    assert!(!should_occlude_underground(DimId::Nether, 70.0, 64, 0, false));
    assert!(!should_occlude_underground(DimId::End, 70.0, 64, 0, false));
    assert_eq!(underground_occlude_ceil(DimId::Nether, 0, 0), None);
    assert_eq!(underground_occlude_ceil(DimId::End, 0, 0), None);
    // `hide_underground = false` turns the feature off alone.
    ac_test_set_underground_occlusion(Some(false));
    assert!(!should_occlude_underground(ow, 70.0, 64, 0, false));
    ac_test_set_underground_occlusion(Some(true));
    // `antixray = false` turns it off too: occlusion is part of anti-xray.
    ac_test_set_anti_xray(Some(false));
    assert!(!should_occlude_underground(ow, 70.0, 64, 0, false));
    ac_test_set_anti_xray(None);

    if crate::test_support::isolated_world_test() {
        ac_test_set_underground_occlusion(None);
        return;
    }
    let _guard = WORLD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = crate::worldgen::lock_gen_for_tests();
    assert!(worldgen::init(0, crate::name_to_state));
    assert!(worldgen::init_dim(DimId::Nether, 0, crate::name_to_state));

    // The encode double-gates on skylight: even asked for the occluded lane, a
    // Nether chunk comes out byte-identical to the real lane — the flag cannot
    // mask open Nether caverns, and both cache keys carry the same bytes.
    let mut gen: GenSources = HashMap::new();
    let occ = chunk_data_lane(0, 0, DimId::Nether, &mut gen, true, true);
    let mut gen: GenSources = HashMap::new();
    let rev = chunk_data_lane(0, 0, DimId::Nether, &mut gen, true, false);
    assert_eq!(occ, rev, "Nether chunks must never be occluded");
    let mut gen: GenSources = HashMap::new();
    let occ_frame = cached_chunk_frame(None, &mut gen, DimId::Nether, 0, 0, true);
    let mut gen: GenSources = HashMap::new();
    let rev_frame = cached_chunk_frame(None, &mut gen, DimId::Nether, 0, 0, false);
    assert_eq!(occ_frame, rev_frame);
    ac_test_set_underground_occlusion(None);
}

/// The ceiling comes from the chunk's lowest surface column, so where the
/// terrain opens — a ravine mouth, an ocean floor — the mask drops with it and
/// the honest player looking down sees real rock, water and cave walls instead
/// of fake stone sealing the opening.
#[test]
fn underground_occlusion_preserves_open_ravines_and_oceans() {
    // The formula: capped at Y=32 over normal terrain, section-aligned, and
    // pulled down 16 under the lowest column so openings stay open.
    assert_eq!(occlusion_ceil_from_hmin(64), 32);
    assert_eq!(occlusion_ceil_from_hmin(200), 32);
    assert_eq!(occlusion_ceil_from_hmin(48), 32);
    assert_eq!(occlusion_ceil_from_hmin(47), 16);
    assert_eq!(occlusion_ceil_from_hmin(32), 16); // ocean floor
    assert_eq!(occlusion_ceil_from_hmin(10), -16); // open ravine mouth
    assert_eq!(occlusion_ceil_from_hmin(-30), -48);
    assert_eq!(occlusion_ceil_from_hmin(-48), -64);

    if crate::test_support::isolated_world_test() {
        return;
    }
    let _guard = WORLD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = crate::worldgen::lock_gen_for_tests();
    assert!(worldgen::init(0, crate::name_to_state));
    ac_test_set_underground_occlusion(Some(true));
    let dim = DimId::Overworld;

    // Find a chunk whose surface genuinely dips: its ceiling must drop below
    // the flat 32 cap, and the sections the lowered ceiling leaves unmasked
    // must keep their real air/water — the ravine is not sealed.
    let mut low = None;
    'scan: for cx in -24..24 {
        for cz in -24..24 {
            if let Some(c) = underground_occlude_ceil(dim, cx, cz) {
                if c < 32 {
                    low = Some((cx, cz, c));
                    break 'scan;
                }
            }
        }
    }
    let (cx, cz, ceil) =
        low.unwrap_or_else(|| panic!("no chunk with a lowered ceiling in the scan"));
    assert!(ceil < 32, "scan picked ceil {ceil}");
    let mut gen_occ: GenSources = HashMap::new();
    let occ = wire_sections_lane(dim, cx, cz, &mut gen_occ, true);
    let mut gen_rev: GenSources = HashMap::new();
    let rev = wire_sections_lane(dim, cx, cz, &mut gen_rev, false);
    let min_y = dim.min_y();
    let mut open = false;
    for (s, ((_, ostates), (_, rstates))) in occ.iter().zip(&rev).enumerate() {
        let sy = min_y + 16 * s as i32;
        if sy + 16 <= ceil {
            continue; // still masked: truly below the lowered ceiling
        }
        if sy < 32 {
            // A section the flat cap would have sealed — the drop saved it.
            open = true;
        }
        assert_eq!(
            ostates, rstates,
            "chunk ({cx},{cz}) ceil {ceil}: section {sy} above it must be real"
        );
    }
    assert!(open, "ceil {ceil} never dropped below a real section");
    ac_test_set_underground_occlusion(None);
}
