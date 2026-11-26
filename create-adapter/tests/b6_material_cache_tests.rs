// SPDX-License-Identifier: LGPL-3.0-or-later

//! Tier B6 conformance tests for the revision-cached Aeronautics material
//! policy: bit-exact legacy mass behavior, explicit precedence, cache reuse,
//! and revision invalidation.

use std::collections::{BTreeMap, BTreeSet};

use create_adapter::aeronautics::{
    material_mass_kg, material_mass_rules, MaterialPolicy, MaterialResolutionSource,
    RigidBodySubWorld,
};
use create_adapter::blockstates::{
    core_create_registry, CreateBlockRegistry, DEFAULT_BASE_STATE_ID,
};

fn registry() -> CreateBlockRegistry {
    core_create_registry(DEFAULT_BASE_STATE_ID)
}

fn assemble(blocks: &[([i32; 3], &str)]) -> RigidBodySubWorld {
    let reg = registry();
    let cells = blocks
        .iter()
        .map(|(pos, id)| (*pos, (*id).to_string()))
        .collect();
    RigidBodySubWorld::assemble(6_006, 0, cells, &reg, None, 100.0).unwrap()
}

fn policy(
    exact: &[(&str, f64)],
    tags: &[(&str, f64)],
    families: &[(&str, f64)],
    compatibility: &[(&str, f64)],
) -> MaterialPolicy {
    MaterialPolicy::new(
        exact
            .iter()
            .map(|(id, mass)| ((*id).to_string(), *mass))
            .collect(),
        tags.iter()
            .map(|(tag, mass)| ((*tag).to_string(), *mass))
            .collect(),
        families
            .iter()
            .map(|(family, mass)| ((*family).to_string(), *mass))
            .collect(),
        compatibility
            .iter()
            .map(|(needle, mass)| ((*needle).to_string(), *mass))
            .collect(),
        77.0,
    )
    .unwrap()
}

#[test]
fn shipped_policy_preserves_all_66_ordered_compatibility_rules() {
    let rules = material_mass_rules();
    assert_eq!(rules.len(), 66);
    let policy = MaterialPolicy::aeronautics(100.0).unwrap();
    assert_eq!(policy.compatibility_rules().len(), rules.len());
    for (policy_rule, legacy_rule) in policy.compatibility_rules().iter().zip(rules) {
        assert_eq!(policy_rule.0, legacy_rule.0);
        assert_eq!(policy_rule.1, legacy_rule.1);
    }

    // One voxel per ordered rule. Even where a rule's sample path also hits
    // an earlier compatibility substring, both sides must choose exactly the
    // same first-match mass. This pins the complete ordered table rather
    // than a hand-picked subset.
    let blocks: Vec<_> = rules
        .iter()
        .enumerate()
        .map(|(i, (needle, _))| ([i as i32, 0, 0], format!("parity:{needle}")))
        .collect();
    let cells: BTreeMap<_, _> = blocks.iter().map(|(pos, id)| (*pos, id.clone())).collect();
    let reg = registry();
    let ship = RigidBodySubWorld::assemble(6_066, 0, cells.clone(), &reg, None, 100.0).unwrap();

    let mut expected_mass = 0.0;
    let mut expected_com = [0.0; 3];
    for (pos, id) in &cells {
        let mass = material_mass_kg(id, 100.0);
        expected_mass += mass;
        for axis in 0..3 {
            expected_com[axis] += mass * (f64::from(pos[axis]) + 0.5);
        }
    }
    for component in &mut expected_com {
        *component /= expected_mass;
    }
    assert_eq!(ship.mass, expected_mass);
    assert_eq!(ship.center_of_mass, expected_com);

    // Hard-pinned regression totals for the complete 66-rule corpus. These
    // are intentionally exact values: the cache must not perturb accumulation
    // order or a single bit of the mass/inertia result.
    assert_eq!(expected_mass, 24_965.0);
    assert_eq!(ship.inertia_tensor[0][0], 4_160.833_333_333_333);
    assert_eq!(ship.inertia_tensor[1][1], 3_953_701.471_827_226_7);
    assert_eq!(ship.inertia_tensor[2][2], 3_953_701.471_827_226_7);
}

#[test]
fn descriptor_precedence_is_exact_then_tag_then_family_then_compat_then_default() {
    let compatibility = [("wool", 15.0), ("mystery", 44.0)];
    let exact_policy = policy(
        &[("minecraft:white_wool", 111.0)],
        &[("minecraft:lightweight", 222.0)],
        &[("wool", 133.0)],
        &compatibility,
    );
    let resolved = exact_policy.resolve(
        "minecraft:white_wool[some=state]",
        &["minecraft:lightweight"],
        &["wool"],
    );
    assert_eq!(resolved.mass_kg, 111.0);
    assert_eq!(resolved.source, MaterialResolutionSource::ExactOverride);

    let tag_policy = policy(
        &[],
        &[("minecraft:lightweight", 222.0)],
        &[("wool", 133.0)],
        &compatibility,
    );
    let resolved = tag_policy.resolve(
        "minecraft:white_wool",
        &["minecraft:lightweight"],
        &["wool"],
    );
    assert_eq!(resolved.mass_kg, 222.0);
    assert_eq!(resolved.source, MaterialResolutionSource::Tag);

    let family_policy = policy(&[], &[], &[("wool", 133.0)], &compatibility);
    let resolved = family_policy.resolve("minecraft:white_wool", &[], &["wool"]);
    assert_eq!(resolved.mass_kg, 133.0);
    assert_eq!(resolved.source, MaterialResolutionSource::MaterialFamily);

    let compat_policy = policy(&[], &[], &[], &compatibility);
    let resolved = compat_policy.resolve("minecraft:white_wool", &[], &[]);
    assert_eq!(resolved.mass_kg, 15.0);
    assert_eq!(resolved.source, MaterialResolutionSource::Compatibility);

    let resolved = compat_policy.resolve("test:unmatched", &[], &[]);
    assert_eq!(resolved.mass_kg, 77.0);
    assert_eq!(resolved.source, MaterialResolutionSource::DefaultFallback);
}

#[test]
fn material_policy_validation_rejects_invalid_masses_and_keys() {
    assert!(MaterialPolicy::aeronautics(f64::NAN).is_err());
    assert!(MaterialPolicy::aeronautics(0.0).is_err());
    assert!(MaterialPolicy::new(
        BTreeMap::new(),
        Vec::new(),
        Vec::new(),
        vec![(String::new(), 1.0)],
        100.0,
    )
    .is_err());
    assert!(MaterialPolicy::new(
        BTreeMap::new(),
        Vec::new(),
        Vec::new(),
        vec![("stone".to_string(), -1.0)],
        100.0,
    )
    .is_err());
    assert!(MaterialPolicy::new(
        [("minecraft:stone[x=y]".to_string(), 1.0)]
            .into_iter()
            .collect(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        100.0,
    )
    .is_err());
}

#[test]
fn override_wins_over_substring_collision_and_revision_invalidates_cache() {
    let mut ship = assemble(&[
        ([0, 0, 0], "minecraft:white_wool"),
        ([1, 0, 0], "minecraft:obsidian"),
    ]);
    assert_eq!(ship.mass, 1_215.0);
    let (_, initial_hits, initial_misses, initial_revision) = ship.material_cache_stats();
    assert_eq!(initial_revision, 0);
    assert!(initial_hits >= 2);
    assert!(initial_misses >= 2);

    assert!(ship.set_block_mass_override("minecraft:white_wool[color=white]", 987.5));
    assert_eq!(ship.mass, 2_187.5);
    assert_eq!(ship.mass_overrides()["minecraft:white_wool"], 987.5);
    let (_, hits_after_override, misses_after_override, override_revision) =
        ship.material_cache_stats();
    assert_eq!(override_revision, 1);
    assert!(misses_after_override > initial_misses);

    // A physics recomputation with no override change must be pure cache hits:
    // misses stay fixed while the mass and inertia accumulation paths hit the
    // resolved per-id entries again.
    ship.recompute_physics();
    let (_, hits_after_recompute, misses_after_recompute, revision_after_recompute) =
        ship.material_cache_stats();
    assert_eq!(revision_after_recompute, override_revision);
    assert_eq!(misses_after_recompute, misses_after_override);
    assert!(hits_after_recompute > hits_after_override);

    assert!(ship.clear_block_mass_override("minecraft:white_wool"));
    assert_eq!(ship.mass, 1_215.0);
    let (_, _, misses_after_clear, clear_revision) = ship.material_cache_stats();
    assert_eq!(clear_revision, 2);
    assert!(misses_after_clear > misses_after_recompute);

    // Inertia is recomputed through the same invalidated effective masses.
    // Trace inertia: the x-separated pair contributes `2*m1*m2/M*d²` to
    // Iyy+Izz, while each cube's `m/6` self-inertia appears once on each of
    // the three diagonals (`M/2` in aggregate).
    let total_mass = 15.0 + 1_200.0;
    assert_eq!(
        ship.inertia_tensor[0][0] + ship.inertia_tensor[1][1] + ship.inertia_tensor[2][2],
        2.0 * 15.0 * 1_200.0 / total_mass + total_mass / 2.0
    );
}

#[test]
fn distinct_blockstates_share_one_canonical_cache_entry_bit_exactly() {
    let mut ship = assemble(&[
        ([0, 0, 0], "minecraft:oak_log[axis=x]"),
        ([1, 0, 0], "minecraft:oak_log[axis=y]"),
        ([2, 0, 0], "minecraft:oak_log[axis=z]"),
    ]);
    assert_eq!(ship.mass, 210.0);
    let (entries, _, _, _) = ship.material_cache_stats();
    assert_eq!(entries, 1);
    ship.recompute_physics();
    let (entries_after, _, misses_after, _) = ship.material_cache_stats();
    assert_eq!(entries_after, 1);
    assert_eq!(misses_after, 1);

    // Distinct override spellings canonicalize to the same exact key.
    assert!(ship.set_block_mass_override("minecraft:oak_log[axis=y]", 90.0));
    assert_eq!(
        ship.mass_overrides().keys().collect::<BTreeSet<_>>(),
        BTreeSet::from([&"minecraft:oak_log".to_string()])
    );
    assert_eq!(ship.mass, 270.0);
}
