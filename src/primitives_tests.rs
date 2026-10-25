use super::*;

fn cases(kind: &str) -> Vec<json::J> {
    let fixture = json::jparse(include_bytes!(
        "../evidence/foundation/valence-primitives/vanilla.json"
    ))
    .unwrap();
    assert_eq!(fixture.get("minecraft").unwrap().str(), "1.21.1");
    let rows: Vec<_> = fixture
        .get("cases")
        .unwrap()
        .as_arr()
        .unwrap()
        .iter()
        .filter(|j| j.get("kind").unwrap().str() == kind)
        .cloned()
        .collect();
    assert!(!rows.is_empty(), "empty oracle coverage for {kind}");
    rows
}
fn ints(row: &json::J, key: &str) -> Vec<i64> {
    row.get(key)
        .unwrap()
        .as_arr()
        .unwrap()
        .iter()
        .map(|j| j.num() as i64)
        .collect()
}
fn assert_encoding(row: &json::J, wire: &[u8]) {
    let want = row.get("expected").unwrap();
    let hash: String = sha1(wire).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        (wire.len(), hash.as_str()),
        (
            want.get("length").unwrap().num() as usize,
            want.get("sha1").unwrap().str()
        ),
        "{}",
        row.get("id").unwrap().str()
    );
}

#[test]
fn primitives_aabb_clip_matches_native_finite_segments() {
    for row in cases("clip") {
        let numbers = |key: &str| -> Vec<f64> {
            row.get(key)
                .unwrap()
                .as_arr()
                .unwrap()
                .iter()
                .map(|j| j.num())
                .collect()
        };
        let box_values = numbers("box");
        let bounds = Aabb::new(
            box_values[..3].try_into().unwrap(),
            box_values[3..].try_into().unwrap(),
        );
        let start: [f64; 3] = numbers("start").try_into().unwrap();
        let end: [f64; 3] = numbers("end").try_into().unwrap();
        let got: Vec<_> = bounds
            .clip(start, end)
            .into_iter()
            .flatten()
            .map(|v| format!("{:016x}", v.to_bits()))
            .collect();
        let want: Vec<_> = row
            .get("expected")
            .unwrap()
            .get("hit_bits")
            .unwrap()
            .as_arr()
            .unwrap()
            .iter()
            .map(|j| j.str().to_string())
            .collect();
        assert_eq!(got, want, "{}", row.get("id").unwrap().str());
    }
}

#[test]
fn primitives_nbt_writers_match_native_modified_utf8() {
    let mut checked = 0;
    for row in cases("string") {
        let units: Vec<_> = ints(&row, "units").into_iter().map(|v| v as u16).collect();
        // Isolated-surrogate cases belong to the opaque-lore preservation gate,
        // not Rust String's valid-Unicode writer contract.
        if let Ok(text) = String::from_utf16(&units) {
            let text = text.repeat(row.get("repeat").unwrap().num() as usize);
            let mut wire = Vec::new();
            nbt_str(&mut wire, &text);
            assert_encoding(&row, &wire);
            let tagged = nbt_string(&text);
            assert_eq!(tagged[0], 8);
            assert_eq!(&tagged[1..], wire);
            let chat = chat_nbt(&text);
            assert_eq!(&chat[8..chat.len() - 1], wire);
            checked += 1;
        }
    }
    assert_eq!(checked, 9);
}

#[test]
fn primitives_nbt_lore_append_preserves_java_string_bytes() {
    let mut checked = 0;
    for row in cases("string") {
        let Some(hex) = row.get("expected").unwrap().get("hex") else {
            continue;
        };
        let hex = hex.str();
        let mut tagged = vec![8];
        for i in (0..hex.len()).step_by(2) {
            tagged.push(u8::from_str_radix(&hex[i..i + 2], 16).unwrap());
        }
        let mut lore = vec![1];
        lore.extend_from_slice(&tagged);
        let mut stack = (1, 1, vec![(COMP_LORE, lore.clone())]);
        apply_loot_funcs(&mut stack, &[LootFunc::SetLore(vec!["new".into()], true)]);
        let mut expected = vec![2];
        expected.extend_from_slice(&tagged);
        expected.extend_from_slice(&nbt_string("new"));
        assert_eq!(
            comp_get(&stack.2, COMP_LORE),
            Some(expected.as_slice()),
            "{}",
            row.get("id").unwrap().str()
        );
        // Persistence must keep the same opaque component payload.
        let mut saved = Vec::new();
        write_comps(&mut saved, &stack.2);
        assert_eq!(read_comps(&saved, &mut 0), stack.2);
        checked += 1;
    }
    assert_eq!(checked, 9);
}

#[test]
fn primitives_nbt_bounded_prefix_does_not_split_a_scalar() {
    for text in [
        "a".repeat(65536),
        char::from(0).to_string().repeat(32768),
        "💣".repeat(10923),
        format!("{}💣z", "a".repeat(65530)),
    ] {
        let tagged = nbt_string(&text);
        let length = u16::from_be_bytes([tagged[1], tagged[2]]) as usize;
        assert_eq!(tagged.len(), length + 3);
        assert!(length <= 65535);
        let mut writer = Vec::new();
        nbt_str(&mut writer, &text);
        assert_eq!(&tagged[1..], writer);
        let expected = if text.starts_with('a') {
            if text.len() == 65536 {
                65535
            } else {
                65530
            }
        } else if text.starts_with(char::from(0)) {
            65534
        } else {
            65532
        };
        assert_eq!(length, expected);
    }
}

#[test]
fn primitives_section_encoding_matches_native_palettes() {
    for row in cases("section") {
        let states = ints(&row, "states");
        let biomes = ints(&row, "biomes");
        let wire = section_encode(
            &std::array::from_fn(|i| states[i % states.len()] as u32),
            &std::array::from_fn(|i| biomes[i % biomes.len()]),
        );
        assert_encoding(&row, &wire);
    }
}
