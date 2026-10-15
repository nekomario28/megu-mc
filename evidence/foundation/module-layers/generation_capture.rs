use super::*;

#[test]
fn capture_generation_outputs() {
    let mut g = Gen::load(0, crate::name_to_state).expect("real datapack fixture");
    let mut bytes = b"GEN-OUTPUT-1\0".to_vec();
    for (x, y, z) in [(-1, 64, -1), (0, 64, 0), (-17, 64, -1), (0, 80, 0)] {
        let result = g.feat_state(x, y, z, crate::feature::populate);
        for n in [x, y, z] { bytes.extend_from_slice(&n.to_le_bytes()); }
        bytes.push(u8::from(result.is_some()));
        bytes.extend_from_slice(&result.unwrap_or(0).to_le_bytes());
        let mut rows: Vec<_> = g.feat_c.iter().flat_map(|(&(cx, cz), map)| {
            map.iter().map(move |(&index, &state)| (cx, cz, index, state))
        }).collect();
        rows.sort_unstable();
        assert!(!rows.is_empty(), "the fixture must exercise feature writes");
        bytes.extend_from_slice(&(rows.len() as u64).to_le_bytes());
        for (cx, cz, index, state) in rows {
            bytes.extend_from_slice(&cx.to_le_bytes());
            bytes.extend_from_slice(&cz.to_le_bytes());
            bytes.extend_from_slice(&index.to_le_bytes());
            bytes.extend_from_slice(&state.to_le_bytes());
        }
        let mut done: Vec<_> = g.pop_done.iter().copied().collect();
        done.sort_unstable();
        bytes.extend_from_slice(&(done.len() as u64).to_le_bytes());
        for (cx, cz) in done {
            bytes.extend_from_slice(&cx.to_le_bytes());
            bytes.extend_from_slice(&cz.to_le_bytes());
        }
    }
    let out = std::env::var("MC_SPINE_CAPTURE_OUT").expect("owned output path required");
    std::fs::write(out, &bytes).expect("write capture");
    eprintln!("generation output capture: {} bytes", bytes.len());
}
