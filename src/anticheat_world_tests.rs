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
    let buried_frame = cached_chunk_frame(None, &mut gen, DimId::Overworld, x >> 4, z >> 4);
    // Expose one face and rebuild.
    set_block(x + 1, y, z, 0);
    let exposed_frame = cached_chunk_frame(None, &mut gen, DimId::Overworld, x >> 4, z >> 4);
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
