// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
use mc_spine_kernel::{
    collide, fluid_impulse, nbt_read_gzip_validate, nbt_read_named_validate, nbt_roundtrip_named,
    Aabb, CommandDispatcher, Error, FluidCell, FluidKind, FluidQuery, LightEdit, LightEngine,
    LightFact, LightKind, PalettedSection, Registry, SECTION_CELLS, STATE_COUNT,
};

fn varint(input: &mut &[u8]) -> u32 {
    let mut value = 0;
    for shift in (0..35).step_by(7) {
        let (byte, rest) = input.split_first().unwrap();
        *input = rest;
        value |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return value;
        }
    }
    panic!("unterminated VarInt");
}

#[test]
fn section_wire_every_width_non_straddling_and_zero_padded() {
    for (distinct, bits) in [(2, 4), (17, 5), (33, 6), (65, 7), (129, 8), (257, 15)] {
        // Set in reverse order so the canonical wire palette must be reordered.
        let states: [u32; SECTION_CELLS] = std::array::from_fn(|i| {
            let state = (i % distinct) as u32;
            if state == 1 {
                STATE_COUNT - 1
            } else {
                state
            }
        });
        let mut section = PalettedSection::new_single(0).unwrap();
        for (i, &state) in states.iter().enumerate().rev() {
            section.set(i, state).unwrap();
        }
        let mut unpacked = [0; SECTION_CELLS];
        section.unpack_into(&mut unpacked);
        assert_eq!(unpacked, states);
        assert_eq!(section.palette_bits(), bits);
        assert_eq!(
            usize::from(section.non_air()),
            states.iter().filter(|&&state| state != 0).count()
        );
        assert!(!section.is_uniform());

        let mut wire = Vec::new();
        section.write_block_states_wire(&mut wire);
        assert_eq!(wire[0], bits);
        let mut wire = &wire[1..];
        if bits < 15 {
            assert_eq!(varint(&mut wire) as usize, distinct);
            for &state in &states[..distinct] {
                assert_eq!(varint(&mut wire), state);
            }
        }
        let per_word = 64 / usize::from(bits);
        assert_eq!(varint(&mut wire) as usize, SECTION_CELLS.div_ceil(per_word));
        let (words, rest) = wire.as_chunks::<8>();
        assert!(rest.is_empty());
        assert_eq!(words.len(), SECTION_CELLS.div_ceil(per_word));
        for (word_index, (&word, cells)) in words.iter().zip(states.chunks(per_word)).enumerate() {
            let mut word = u64::from_be_bytes(word);
            for (offset, &state) in cells.iter().enumerate() {
                let expected = if bits == 15 {
                    state
                } else {
                    ((word_index * per_word + offset) % distinct) as u32
                };
                assert_eq!((word & ((1 << bits) - 1)) as u32, expected);
                word >>= bits;
            }
            assert_eq!(word, 0, "wire padding at width {bits}, word {word_index}");
        }
    }
}

#[test]
fn section_checked_updates_and_reverted_wire() {
    assert!(matches!(
        PalettedSection::new_single(STATE_COUNT),
        Err(Error::InvalidState)
    ));
    let mut section = PalettedSection::new_single(12958).unwrap();
    assert_eq!(section.uniform_state(), Some(12958));
    assert_eq!(section.get(SECTION_CELLS), Err(Error::OutOfRange));
    assert_eq!(section.set(SECTION_CELLS, 1), Err(Error::OutOfRange));
    assert_eq!(section.set(0, u32::MAX), Err(Error::InvalidState));
    assert_eq!(section.get(0), Ok(12958));
    section.set(4095, 12959).unwrap();
    assert_eq!(section.non_air(), 0);
    section.set(4095, 1).unwrap();
    assert_eq!(section.non_air(), 1);
    section.set(4095, 12958).unwrap();
    assert!(section.is_uniform());
    assert_eq!(section.uniform_state(), None);

    let mut wire = Vec::new();
    section.write_block_states_wire(&mut wire);
    assert_eq!(wire, [0, 0x9e, 0x65, 0]);
}

fn hex(text: &str) -> Vec<u8> {
    let (pairs, rest) = text.as_bytes().as_chunks::<2>();
    assert!(rest.is_empty());
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

#[test]
fn named_nbt_matches_native_codec_vectors() {
    for row in include_str!("../../tests/data/nbt_codec_1211.tsv").lines() {
        let (input, expected) = row.split_once('\t').unwrap();
        let input = hex(input);
        if expected == "REJECT" {
            assert!(nbt_read_named_validate(&input).is_err(), "{row}");
        } else {
            assert_eq!(nbt_roundtrip_named(&input), Ok(hex(expected)), "{row}");
        }
    }
    let nested = |depth| {
        let mut nbt = [10, 0, 0].repeat(depth);
        nbt.resize(nbt.len() + depth, 0);
        nbt
    };
    assert_eq!(nbt_read_named_validate(&nested(512)), Ok(()));
    assert_eq!(nbt_read_named_validate(&nested(513)), Err(Error::NbtDepth));
    assert_eq!(
        nbt_read_named_validate(&[10, 0, 0, 0, 0]),
        Err(Error::InvalidNbt)
    );
}

#[test]
fn gzip_nbt_stored_fixed_dynamic_and_optional_headers() {
    // Python zlib-generated members. The dynamic case has a 4096-byte NBT byte array.
    for (gzip, expected_len) in [
        ("1f8b0800000000000403010400fbff0a000000783ff94e04000000", 4),
        ("1f8b0800000000000403e36260600000783ff94e04000000", 4),
        ("1f8b081e0000000004030200616273616d706c6500636f6d6d656e7400f5a4e36260600000783ff94e04000000", 4),
        ("1f8b0800000000000203edc5c10d00200800317e7e5d5191fd579039482f977445b7e3dc7c7d9124499224c9b97e29b9e9ce07100000", 4103),
        (concat!("1f8b0800000000000403e362000075fa36bb02000000", "1f8b080000000000040363600000ff12d94102000000"), 4),
    ] {
        let mut gzip = hex(gzip);
        assert_eq!(nbt_read_gzip_validate(&gzip, expected_len), Ok(()));
        assert_eq!(nbt_read_gzip_validate(&gzip, expected_len - 1), Err(Error::ByteBudget));
        assert_eq!(nbt_read_gzip_validate(&gzip, usize::MAX), Ok(()));
        let crc_start = gzip.len() - 8;
        gzip[crc_start] ^= 1;
        assert_eq!(nbt_read_gzip_validate(&gzip, expected_len), Err(Error::InvalidGzip));
    }
}

#[test]
fn gzip_rejects_truncation_checksum_header_and_trailing_data() {
    let mut gzip = hex("1f8b081e0000000004030200616273616d706c6500636f6d6d656e7400f5a4e36260600000783ff94e04000000");
    for end in 0..gzip.len() {
        assert_eq!(
            nbt_read_gzip_validate(&gzip[..end], 4),
            Err(Error::InvalidGzip)
        );
    }
    gzip.push(0);
    assert_eq!(nbt_read_gzip_validate(&gzip, 4), Err(Error::InvalidGzip));
    gzip.pop();
    gzip[12] ^= 1;
    assert_eq!(nbt_read_gzip_validate(&gzip, 4), Err(Error::InvalidGzip));
    let mut gzip = hex("1f8b0800000000000403e3626060600000bcef928c05000000");
    assert_eq!(nbt_read_gzip_validate(&gzip, 5), Err(Error::InvalidNbt));
    gzip[10] |= 0b110;
    assert_eq!(nbt_read_gzip_validate(&gzip, 5), Err(Error::InvalidGzip));
}

#[test]
fn completion_uses_utf8_byte_ranges_and_stable_brigadier_order() {
    let mut commands = CommandDispatcher::new();
    for name in ["Élan", "éclair", "Écho", "écho", "say"] {
        commands.register_literal(name).unwrap();
    }
    assert_eq!(
        commands.register_literal("say hi"),
        Err(Error::InvalidArgument)
    );
    assert_eq!(commands.register_literal(""), Err(Error::InvalidArgument));
    assert!(commands.parse("éclair").executable);
    assert_eq!(commands.parse("éclair").cursor, 7);
    assert!(!commands.parse("éclair x").executable);
    assert!(!commands.parse("unknown").executable);

    let input = "écl xyz";
    for _ in 0..3 {
        let suggestions = commands.complete_suggestions(input, 2).unwrap();
        assert_eq!(suggestions.range(), 0..2);
        assert_eq!(
            (0..suggestions.len())
                .filter_map(|i| suggestions.get(i))
                .collect::<Vec<_>>(),
            ["Écho", "écho", "éclair", "Élan"]
        );
    }
    assert!(matches!(
        commands.complete_suggestions(input, 1),
        Err(Error::OutOfRange)
    ));
    assert!(matches!(
        commands.complete_suggestions(input, input.len() + 1),
        Err(Error::OutOfRange)
    ));
}

fn bounds(min: [f64; 3], max: [f64; 3]) -> Aabb {
    Aabb { min, max }
}

#[test]
fn collision_orders_axes_and_preserves_eps_flags() {
    let actor = bounds([0.0; 3], [1.0; 3]);
    let pillar = [bounds([1.0, 0.0, 1.0], [2.0, 1.0, 2.0])];
    for (delta, resolved) in [
        ([0.2, 0.0, 0.5], [0.0, 0.0, 0.5]),
        ([0.5, 0.0, 0.5], [0.5, 0.0, 0.0]),
    ] {
        let result = collide(actor, delta, false, 0.0, &pillar).unwrap();
        assert_eq!(result.delta, resolved);
        assert!(result.horizontal_collision);
    }
    let wall = [bounds([1.0, 0.0, 0.0], [2.0, 1.0, 1.0])];
    let delta = [1.0e-8, 0.0, 0.0];
    let result = collide(actor, delta, true, 0.0, &wall).unwrap();
    assert_eq!(result.delta, [0.0; 3]);
    assert!(!result.horizontal_collision);
    assert_eq!(collide(actor, delta, false, 0.0, &[]).unwrap().delta, delta);
}

#[test]
fn collision_steps_over_slab_and_respects_limit_headroom_and_grounding() {
    let actor = bounds([0.0; 3], [1.0; 3]);
    let step = bounds([1.0, 0.0, 0.0], [2.0, 0.5, 1.0]);
    let ceiling = bounds([1.0, 1.25, 0.0], [2.0, 2.0, 1.0]);
    let floor = bounds([-1.0; 3], [3.0, 0.0, 3.0]);
    let delta = [0.5, 0.0, 0.0];
    let result = collide(actor, delta, true, 0.5, &[step]).unwrap();
    assert!(result.stepped_up && result.vertical_collision && !result.horizontal_collision);
    assert_eq!(result.delta, [0.5, 0.5, 0.0]);
    for (ground, height, shapes) in [
        (true, 0.49, [step, floor]),
        (false, 0.5, [step, floor]),
        (true, 0.5, [step, ceiling]),
    ] {
        let result = collide(actor, delta, ground, height, &shapes).unwrap();
        assert!(result.horizontal_collision && !result.stepped_up);
        assert_eq!(result.delta, [0.0; 3]);
    }
    let falling = bounds([0.0, 0.125, 0.0], [1.0, 1.125, 1.0]);
    let result = collide(falling, [0.5, -0.25, 0.0], false, 0.5, &[floor, step]).unwrap();
    assert!(result.stepped_up && result.vertical_collision_below);
    assert_eq!(result.delta, [0.5, 0.375, 0.0]);
    assert_eq!(
        collide(actor, [f64::NAN; 3], true, 0.5, &[step]),
        Err(Error::InvalidArgument)
    );
}

#[test]
fn fluid_uses_snapshot_facts_for_push_height_and_sturdy_faces() {
    let actor = bounds([0.0, 64.0, 0.0], [1.0, 65.0, 1.0]);
    let cell = |kind, amount| FluidCell {
        kind: Some(kind),
        amount,
        ..FluidCell::default()
    };
    let mut query = FluidQuery {
        kind: FluidKind::Water,
        ultra_warm: false,
        pushed_by_fluid: true,
        is_player: true,
        velocity: [0.0; 3],
    };
    let flow = |kind| {
        move |x, y, z| {
            Ok(match (x, y, z) {
                (0, 64, 0) => cell(kind, 8),
                (1, 64, 0) => cell(kind, 7),
                _ => FluidCell::default(),
            })
        }
    };
    let result = fluid_impulse(actor, query, flow(query.kind)).unwrap();
    assert!(result.in_fluid);
    assert_eq!(
        result.max_height,
        f64::from(64.0f32 + 8.0f32 / 9.0) - 64.001
    );
    assert_eq!(result.push, [0.014, 0.0, 0.0]);

    query.kind = FluidKind::Lava;
    query.ultra_warm = true;
    assert_eq!(
        fluid_impulse(actor, query, flow(query.kind)).unwrap().push,
        [0.007, 0.0, 0.0]
    );
    query.ultra_warm = false;
    assert_eq!(
        fluid_impulse(actor, query, flow(query.kind)).unwrap().push,
        [0.0045000000000000005, 0.0, 0.0]
    );
    query.pushed_by_fluid = false;
    assert_eq!(
        fluid_impulse(actor, query, flow(query.kind)).unwrap().push,
        [0.0; 3]
    );

    query.kind = FluidKind::Water;
    query.pushed_by_fluid = true;
    for (is_ice, expected_y) in [(false, -0.014), (true, 0.0)] {
        let result = fluid_impulse(actor, query, |x, y, z| {
            Ok(match (x, y, z) {
                (0, 64, 0) => FluidCell {
                    falling: true,
                    ..cell(FluidKind::Water, 8)
                },
                (0, 65, 0) => cell(FluidKind::Water, 8),
                (1, 64, 0) => FluidCell {
                    sturdy_faces: 1 << 5,
                    blocks_motion: true,
                    is_ice,
                    ..FluidCell::default()
                },
                _ => FluidCell::default(),
            })
        })
        .unwrap();
        assert_eq!(result.max_height, 65.0 - 64.001);
        assert_eq!(result.push, [0.0, expected_y, 0.0]);
    }
    assert_eq!(
        fluid_impulse(actor, query, |_, _, _| Err(Error::QueryFailed)),
        Err(Error::QueryFailed)
    );
    assert_eq!(
        fluid_impulse(actor, query, |_, _, _| Ok(cell(FluidKind::Water, 9))),
        Err(Error::InvalidArgument)
    );
}

fn section_from(states: &[u32; SECTION_CELLS]) -> PalettedSection {
    let mut section = PalettedSection::new_single(states[0]).unwrap();
    for (i, &state) in states.iter().enumerate() {
        section.set(i, state).unwrap();
    }
    section
}

#[test]
fn wire_decode_roundtrips_every_width_and_reports_consumed_bytes() {
    for distinct in [1usize, 2, 17, 33, 65, 129, 257] {
        let states: [u32; SECTION_CELLS] = std::array::from_fn(|i| ((i * 7) % distinct) as u32 * 3);
        let section = section_from(&states);
        let mut wire = Vec::new();
        section.write_block_states_wire(&mut wire);
        let canonical = wire.len();
        wire.extend_from_slice(&[0xaa, 0xbb]);
        let (decoded, consumed) = PalettedSection::decode_block_states_wire(&wire).unwrap();
        assert_eq!(consumed, canonical);
        let mut cells = [0; SECTION_CELLS];
        decoded.unpack_into(&mut cells);
        assert_eq!(cells, states);
        assert_eq!(decoded.non_air(), section.non_air());
        let mut again = Vec::new();
        decoded.write_block_states_wire(&mut again);
        assert_eq!(again, wire[..canonical]);
    }
}

#[test]
fn wire_decode_rejects_malformed_containers_without_panicking() {
    let states: [u32; SECTION_CELLS] = std::array::from_fn(|i| (i % 5) as u32);
    let mut wire = Vec::new();
    section_from(&states).write_block_states_wire(&mut wire);
    for end in 0..wire.len() {
        assert_eq!(
            PalettedSection::decode_block_states_wire(&wire[..end]).err(),
            Some(Error::InvalidWire),
            "truncated at {end}"
        );
    }
    let decode = |bytes: &[u8]| PalettedSection::decode_block_states_wire(bytes).err();
    assert_eq!(
        decode(&[0, 0xff, 0xff, 0xff, 0xff, 0x0f, 0]),
        Some(Error::InvalidWire)
    );
    assert_eq!(decode(&[0, 0xbc, 0xd0, 0x01, 0]), Some(Error::InvalidState));
    assert_eq!(decode(&[0, 0, 1]), Some(Error::InvalidWire));
    for bits in [1, 3, 9, 14, 16, 255] {
        assert_eq!(decode(&[bits, 1, 0, 0]), Some(Error::InvalidWire));
    }
    // Palette length: empty, larger than 2^bits, and an unsupported VarInt width.
    assert_eq!(decode(&[4, 0, 0]), Some(Error::InvalidWire));
    assert_eq!(decode(&[4, 17]), Some(Error::InvalidWire));
    assert_eq!(
        decode(&[4, 0x80, 0x80, 0x80, 0x80, 0x80, 0]),
        Some(Error::InvalidWire)
    );
    // A palette state outside the registry.
    let mut bad_state = wire.clone();
    bad_state[2] = 0xff;
    bad_state.insert(3, 0xff);
    bad_state.insert(4, 0x7f);
    assert_eq!(decode(&bad_state), Some(Error::InvalidState));
    // Wrong word count and an index beyond the palette.
    let mut short = wire.clone();
    let count_at = 2 + 5;
    short[count_at] -= 1;
    assert_eq!(decode(&short), Some(Error::InvalidWire));
    let mut index = wire.clone();
    index[count_at + 1 + 7] |= 0x0f;
    assert_eq!(decode(&index), Some(Error::InvalidWire));
    // Direct values are checked against the registry.
    let mut direct = vec![15, 0x80, 0x08];
    direct.extend(std::iter::repeat_n(0xff, 1024 * 8));
    assert_eq!(decode(&direct), Some(Error::InvalidState));
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

fn column(height: usize, fill: impl Fn(usize, usize, usize, usize) -> LightFact) -> Vec<LightFact> {
    (0..height * 256)
        .map(|i| fill(i & 15, i >> 8, (i >> 4) & 15, i))
        .collect()
}

fn nibble(section: &[u8], x: usize, y: usize, z: usize) -> u8 {
    let cell = y << 8 | z << 4 | x;
    (section[cell / 2] >> (cell % 2 * 4)) & 0xf
}

#[test]
fn light_engine_lights_and_edits_through_the_server_algorithm() {
    assert!(LightEngine::new(1, 16, true).is_err());
    assert!(LightEngine::new(0, 0, true).is_err());
    assert!(LightEngine::new(0, 17, true).is_err());
    assert!(LightEngine::new(0, 16 * 63, true).is_err());
    let mut engine = LightEngine::new(0, 32, true).unwrap();
    assert_eq!(engine.section_count(), 4);
    assert_eq!(
        engine.load_chunk(0, 0, &[AIR; 3]).err(),
        Some(Error::OutOfRange)
    );
    let bad = LightFact { opacity: 16, ..AIR };
    assert_eq!(
        engine.load_chunk(0, 0, &column(32, |_, _, _, _| bad)).err(),
        Some(Error::InvalidArgument)
    );
    assert_eq!(
        engine.section(0, 0, LightKind::Sky, 0).err(),
        Some(Error::OutOfRange)
    );
    engine
        .load_chunk(0, 0, &column(32, |_, _, _, _| AIR))
        .unwrap();
    engine
        .load_chunk(1, 0, &column(32, |_, _, _, _| AIR))
        .unwrap();
    // Open sky is full-bright, including the guard section above the dimension.
    for index in 1..4 {
        let sky = engine.section(0, 0, LightKind::Sky, index).unwrap();
        assert!(sky.iter().all(|&byte| byte == 0xff), "section {index}");
    }
    assert!(engine
        .section(0, 0, LightKind::Block, 1)
        .unwrap()
        .iter()
        .all(|&b| b == 0));
    assert_eq!(
        engine.section(0, 0, LightKind::Block, 4).err(),
        Some(Error::OutOfRange)
    );

    let torch = LightFact {
        emission: 14,
        ..AIR
    };
    let edit = |x, y, z, fact| LightEdit { x, y, z, fact };
    let update = engine.apply_edits(&[edit(15, 4, 4, torch)]).unwrap();
    assert!(update.invalidated_cells > 0);
    let chunks: Vec<_> = update.chunks.iter().map(|chunk| chunk.chunk).collect();
    assert_eq!(chunks, [(0, 0), (1, 0)]);
    assert_eq!(update.chunks[0].block_mask, 0b110);
    let section = &update.chunks[0].block_sections[0];
    assert_eq!(section.0, 1);
    assert_eq!(nibble(&section.1, 15, 4, 4), 14);
    assert_eq!(nibble(&section.1, 12, 4, 4), 11);
    assert_eq!(
        nibble(&engine.section(1, 0, LightKind::Block, 1).unwrap(), 0, 4, 4),
        13
    );

    // Removing the emitter darkens the section, reported through the empty mask.
    let update = engine.apply_edits(&[edit(15, 4, 4, AIR)]).unwrap();
    assert!(update
        .chunks
        .iter()
        .all(|c| c.block_sections.is_empty() && c.block_mask == 0));
    assert!(update.chunks.iter().all(|c| c.empty_block_mask == 0b110));

    // A roof shades the column below it and only reports the sky layer.
    let roof: Vec<_> = (0..16)
        .flat_map(|x| (0..16).map(move |z| edit(x, 31, z, STONE)))
        .collect();
    let update = engine.apply_edits(&roof).unwrap();
    assert!(update
        .chunks
        .iter()
        .any(|c| c.sky_mask != 0 || c.empty_sky_mask != 0));
    let sky = engine.section(0, 0, LightKind::Sky, 1).unwrap();
    assert_eq!(nibble(&sky, 0, 0, 0), 0);
}

#[test]
fn light_edits_are_atomic_and_fail_closed() {
    let mut engine = LightEngine::new(-16, 32, true).unwrap();
    engine
        .load_chunk(-1, -1, &column(32, |_, _, _, _| AIR))
        .unwrap();
    let torch = LightFact {
        emission: 15,
        ..AIR
    };
    let edit = |x, y, z, fact| LightEdit { x, y, z, fact };
    assert_eq!(engine.apply_edits(&[]).err(), Some(Error::InvalidArgument));
    let good = edit(-1, -16, -1, torch);
    for bad in [
        edit(-1, 16, -1, torch),
        edit(-1, -17, -1, torch),
        edit(16, 0, -1, torch),
        edit(
            -1,
            0,
            -1,
            LightFact {
                occlusion: [0x10; 6],
                ..torch
            },
        ),
    ] {
        assert!(engine.apply_edits(&[good, bad]).is_err());
    }
    // The rejected batches left the window untouched.
    assert!(engine
        .section(-1, -1, LightKind::Block, 1)
        .unwrap()
        .iter()
        .all(|&b| b == 0));
    let update = engine.apply_edits(&[good]).unwrap();
    assert_eq!(update.chunks.len(), 1);
    assert_eq!(update.chunks[0].chunk, (-1, -1));
    assert_eq!(
        nibble(
            &engine.section(-1, -1, LightKind::Block, 1).unwrap(),
            15,
            0,
            15
        ),
        15
    );
}

#[test]
fn light_without_skylight_keeps_sky_dark() {
    let mut engine = LightEngine::new(0, 16, false).unwrap();
    engine
        .load_chunk(0, 0, &column(16, |_, _, _, _| AIR))
        .unwrap();
    for index in 0..3 {
        assert!(engine
            .section(0, 0, LightKind::Sky, index)
            .unwrap()
            .iter()
            .all(|&b| b == 0));
    }
}

fn open_registry() -> Registry {
    let read = |name: &str| {
        std::fs::read(format!("{}/../assets/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    };
    Registry::open_json(&read("block_states_1211.json"), &read("blocks_1211.json")).unwrap()
}

#[test]
fn registry_facts_feed_the_light_engine_and_agree_with_palette_ids() {
    let registry = open_registry();
    assert_eq!(registry.state_count(), STATE_COUNT);
    let stone = registry.default_state("minecraft:stone").unwrap();
    let glowstone = registry.default_state("minecraft:glowstone").unwrap();
    let to_fact = |state| {
        let facts = registry.facts(state).unwrap();
        LightFact {
            opacity: facts.opacity,
            emission: facts.luminance,
            occlusion: facts.face_occlusion,
        }
    };
    assert_eq!(registry.facts(glowstone).unwrap().luminance, 15);
    assert_eq!(to_fact(stone), STONE);
    let mut engine = LightEngine::new(0, 16, true).unwrap();
    engine
        .load_chunk(
            0,
            0,
            &column(16, |x, y, z, _| {
                if (x, y, z) == (8, 8, 8) {
                    to_fact(glowstone)
                } else {
                    AIR
                }
            }),
        )
        .unwrap();
    assert_eq!(
        nibble(&engine.section(0, 0, LightKind::Block, 1).unwrap(), 8, 8, 8),
        15
    );
    // Section air accounting uses the same state-ID table.
    assert_eq!(registry.facts(0).unwrap().is_air, 1);
    assert_eq!(PalettedSection::new_single(0).unwrap().non_air(), 0);
}
