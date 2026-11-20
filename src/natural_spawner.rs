// R7 GP-2: player-distance rules for the Overworld's nightly spawn cycle.
fn nightly_hostile_spawn_position(tick: u64) -> Option<((f64, f64, f64), u64)> {
    let players: Vec<_> = client_snapshot()
        .into_iter()
        .filter_map(|client| {
            if client.is_spectator() {
                return None;
            }
            let dim = client.dim.lock().unwrap();
            if *dim != DimId::Overworld {
                return None;
            }
            let (x, y, z, ..) = *client.pos.lock().unwrap();
            Some((x, y, z))
        })
        .collect();
    if players.is_empty() {
        return None;
    }

    // Rotate eligible anchors on attempts, independently of the tick cadence.
    let attempt = SPAWN_ATTEMPT.fetch_add(1, Ordering::Relaxed);
    let (px, _, pz) = players[attempt as usize % players.len()];
    let angle = tick as f64 * 2.399;
    let bx = (px + angle.cos() * 32.0).floor() as i32;
    let bz = (pz + angle.sin() * 32.0).floor() as i32;
    let (x, y, z) = (f64::from(bx) + 0.5, surface_y(bx, bz), f64::from(bz) + 0.5);
    let nearest_squared = players
        .iter()
        .map(|&(px, py, pz)| (px - x).powi(2) + (py - y).powi(2) + (pz - z).powi(2))
        .fold(f64::INFINITY, f64::min);
    // NaturalSpawner rejects distances at or below 24 blocks.
    (nearest_squared > 24.0 * 24.0 && nearest_squared <= 128.0 * 128.0)
        .then_some(((x, y, z), attempt))
}
