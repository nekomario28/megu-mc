// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_core::redstone_eda::{
    RedstoneNetlistCache, RedstoneWireSolver, WireLink, WireNode, WirePos,
};
use std::time::Instant;

fn pos(x: i32, y: i32, z: i32) -> WirePos {
    WirePos { x, y, z }
}

fn graph(
    edges: &[(usize, usize)],
    positions: &[WirePos],
    sources: &[(usize, u8)],
) -> Vec<WireNode> {
    let mut links = vec![Vec::new(); positions.len()];
    for &(a, b) in edges {
        links[a].push(WireLink { to: b });
        links[b].push(WireLink { to: a });
    }
    let mut source_power = vec![None; positions.len()];
    for &(index, power) in sources {
        source_power[index] = Some(power);
    }
    positions
        .iter()
        .copied()
        .enumerate()
        .map(|(i, position)| WireNode {
            position,
            power: 0,
            source_power: source_power[i],
            links: std::mem::take(&mut links[i]),
        })
        .collect()
}

/// Deliberately simple repeated fixed-point relaxation oracle.
fn vanilla_relaxation_oracle(nodes: &[WireNode]) -> Vec<u8> {
    let mut power = vec![0_u8; nodes.len()];
    for (i, node) in nodes.iter().enumerate() {
        power[i] = node.source_power.unwrap_or(0).min(15);
    }
    loop {
        let mut changed = false;
        for (i, node) in nodes.iter().enumerate() {
            let next = power[i].saturating_sub(1);
            for link in &node.links {
                if link.to < nodes.len() && next > power[link.to] {
                    power[link.to] = next;
                    changed = true;
                }
            }
        }
        if !changed {
            return power;
        }
    }
}

fn assert_matches_oracle(nodes: Vec<WireNode>) {
    let expected = vanilla_relaxation_oracle(&nodes);
    let mut solver = RedstoneWireSolver::new(nodes);
    solver.solve();
    assert_eq!(
        solver
            .nodes()
            .iter()
            .map(|node| node.power)
            .collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn chains_loops_forks_dual_sources_and_cut_corners_match_fixed_point() {
    let positions = (0..19).map(|x| pos(x, 0, 0)).collect::<Vec<_>>();
    let chain = (0..18).map(|i| (i, i + 1)).collect::<Vec<_>>();
    assert_matches_oracle(graph(&chain[..9], &positions[..10], &[(0, 15)]));

    let loop_edges = [
        (0, 1),
        (1, 2),
        (2, 3),
        (3, 0),
        (1, 4),
        (4, 5),
        (5, 2),
        // Deliberately absent edge (0, 6) models a non-conducting cut corner.
        (6, 7),
        (7, 8),
        (8, 6),
    ];
    assert_matches_oracle(graph(&loop_edges, &positions[..9], &[(0, 14), (8, 9)]));
    assert_matches_oracle(graph(
        &[(0, 1), (0, 2), (1, 3), (2, 3)],
        &positions[..4],
        &[(0, 15)],
    ));
    assert_matches_oracle(graph(
        &[(0, 1), (1, 2), (2, 3)],
        &positions[..4],
        &[(0, 8), (3, 15)],
    ));
}

#[test]
fn step_up_and_step_down_links_are_explicit_and_attenuate_once_per_dust() {
    let staircase = graph(
        &[(0, 1), (1, 2), (2, 3), (3, 4)],
        &[
            pos(0, 0, 0),
            pos(1, 1, 0),
            pos(2, 2, 0),
            pos(3, 1, 0),
            pos(4, 0, 0),
        ],
        &[(0, 15)],
    );
    assert_matches_oracle(staircase);
}

#[test]
fn thousand_node_mega_grid_matches_fixed_point_oracle() {
    let width = 25_usize;
    let height = 40_usize;
    let positions = (0..height)
        .flat_map(|z| (0..width).map(move |x| pos(x as i32, 0, z as i32)))
        .collect::<Vec<_>>();
    let mut edges = Vec::new();
    for z in 0..height {
        for x in 0..width {
            let i = z * width + x;
            if x + 1 < width {
                edges.push((i, i + 1));
            }
            if z + 1 < height {
                edges.push((i, i + width));
            }
        }
    }
    let nodes = graph(
        &edges,
        &positions,
        &[(width * (height / 2) + width / 2, 15)],
    );
    assert_matches_oracle(nodes);
}

#[test]
fn one_update_per_changed_mesh_node_and_sub_millisecond_solve() {
    let width = 25_usize;
    let height = 40_usize;
    let positions = (0..height)
        .flat_map(|z| (0..width).map(move |x| pos(x as i32, 0, z as i32)))
        .collect::<Vec<_>>();
    let mut edges = Vec::new();
    for z in 0..height {
        for x in 0..width {
            let i = z * width + x;
            if x + 1 < width {
                edges.push((i, i + 1));
            }
            if z + 1 < height {
                edges.push((i, i + width));
            }
        }
    }
    let mut nodes = graph(&edges, &positions, &[(0, 15)]);
    let mut solver = RedstoneWireSolver::new(nodes.clone());
    let on = solver.solve();
    assert_eq!(on.updates.len(), on.stats.state_changes);
    assert_eq!(
        on.updates
            .iter()
            .map(|u| u.node)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        on.updates.len()
    );

    nodes[0].source_power = None;
    for (left, right) in solver.nodes_mut().iter_mut().zip(nodes) {
        left.source_power = right.source_power;
    }
    let started = Instant::now();
    let off = solver.solve();
    let elapsed = started.elapsed();
    assert_eq!(off.updates.len(), off.stats.state_changes);
    assert!(off.updates.len() <= 1000);
    assert!(
        elapsed.as_micros() < 1000,
        "1000-node mesh solve took {elapsed:?}"
    );
}

#[test]
fn cache_reuses_revision_and_invalidates_only_touched_components_for_cut_and_bridge() {
    let positions = [pos(0, 0, 0), pos(1, 0, 0), pos(2, 0, 0), pos(10, 0, 0)];
    let original = graph(&[(0, 1), (1, 2)], &positions, &[(0, 15)]);
    let mut cache = RedstoneNetlistCache::new();
    let mut discoveries = 0;
    cache
        .get_or_compile(7, || {
            discoveries += 1;
            original.clone()
        })
        .solve();
    cache
        .get_or_compile(7, || {
            discoveries += 1;
            panic!("stable section revision must reuse compiled topology")
        })
        .solve();
    assert_eq!(discoveries, 1);
    assert_eq!(cache.cached_node_count(), 4);
    let cached_solver = cache.get_or_compile(7, || {
        discoveries += 1;
        panic!("source power changes must not rediscover wire topology")
    });
    cached_solver.nodes_mut()[0].source_power = None;
    let switched_off = cached_solver.solve();
    assert_eq!(switched_off.updates.len(), switched_off.stats.state_changes);
    assert_eq!(discoveries, 1);

    // Cutting the middle dust replaces only its old connected component.
    cache.invalidate_pos(pos(1, 0, 0));
    assert_eq!(cache.dirty_component_count(), 1);
    let cut = graph(&[], &[pos(0, 0, 0), pos(1, 0, 0), pos(2, 0, 0)], &[(0, 15)]);
    let solver = cache.refresh_invalidated(7, |affected| {
        assert!(affected.contains(&pos(0, 0, 0)));
        assert!(affected.contains(&pos(1, 0, 0)));
        assert!(affected.contains(&pos(2, 0, 0)));
        cut.clone()
    });
    solver.solve();
    assert_eq!(cache.cached_node_count(), 4); // unrelated fourth wire survives
    assert_eq!(cache.dirty_component_count(), 0);

    // Bridging neighboring components invalidates both local components; the
    // local discovery returns the newly connected replacement graph.
    let split = graph(&[], &[pos(0, 0, 0), pos(2, 0, 0), pos(10, 0, 0)], &[]);
    cache = RedstoneNetlistCache::new();
    cache.get_or_compile(11, || split.clone());
    cache.invalidate_pos(pos(1, 0, 0));
    assert_eq!(cache.dirty_component_count(), 2);
    let bridged = graph(
        &[(0, 1), (1, 2)],
        &[pos(0, 0, 0), pos(1, 0, 0), pos(2, 0, 0)],
        &[],
    );
    cache.refresh_invalidated(11, |affected| {
        assert!(affected.contains(&pos(0, 0, 0)));
        assert!(affected.contains(&pos(2, 0, 0)));
        bridged.clone()
    });
    assert_eq!(cache.cached_node_count(), 4);
}
