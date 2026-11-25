// SPDX-License-Identifier: LGPL-3.0-or-later
//! Integration tests for Innovation B: fail-closed mesh elision, shared
//! content meshes, frame-paced uploads, and deterministic Merkle restart DST.

use std::sync::Arc;
use std::time::Instant;

use megu_core::coords::{BlockPos, ChunkKey, DimIndex, SectionKey};
use megu_core::dep_graph::{CutoffOutcome, IncrementalDag};
use megu_core::dst_oracle::{
    chunk_leaf_key, run_dst_campaign, verify_merkle_restart, DstInvariantViolation, DstOp, DstRng,
    DstSimulation, INITIAL_TOTAL_ITEMS,
};
use megu_core::mesh_elision::{
    apply_decision_to_dag, compute_section_fingerprint, BlockRenderTraits, ContentHashMeshCache,
    FramePacingUploadRing, RemeshDecision, SectionOcclusionMask, FACE_NEG_X, FACE_NEG_Z,
    FACE_POS_Y,
};

fn full_mask() -> SectionOcclusionMask {
    let mut mask = SectionOcclusionMask::new();
    for x in 0u8..16 {
        for y in 0u8..16 {
            for z in 0u8..16 {
                mask.set_render_traits(x, y, z, BlockRenderTraits::FULL_OPAQUE_CUBE);
            }
        }
    }
    mask
}

fn upload_key(cx: i32) -> SectionKey {
    SectionKey {
        dim: DimIndex::OVERWORLD,
        cx,
        sy: 0,
        cz: 0,
    }
}

#[test]
fn buried_full_opaque_cube_mutation_elides_remesh_completely() {
    let mask = full_mask();
    assert!(mask.is_interior_fully_occluded(8, 8, 8));
    assert!(mask.can_elide_remesh(8, 8, 8, true, true));
    assert!(mask.can_elide_remesh_traits(
        8,
        8,
        8,
        BlockRenderTraits::FULL_OPAQUE_CUBE,
        BlockRenderTraits::FULL_OPAQUE_CUBE,
    ));
    assert_eq!(
        SectionOcclusionMask::classify_section_mutation(
            &mask,
            8,
            8,
            8,
            BlockRenderTraits::FULL_OPAQUE_CUBE,
        ),
        RemeshDecision::Elide,
        "stone -> deepslate style buried full-cube mutation must cost zero remeshing"
    );
}

#[test]
fn exposed_or_border_mutation_never_falsely_elides_and_face_mask_is_exact() {
    let mut exposed = full_mask();
    exposed.set_render_traits(7, 8, 8, BlockRenderTraits::EMPTY);
    assert!(!exposed.is_interior_fully_occluded(8, 8, 8));
    assert_eq!(
        SectionOcclusionMask::classify_section_mutation(
            &exposed,
            8,
            8,
            8,
            BlockRenderTraits::FULL_OPAQUE_CUBE,
        ),
        RemeshDecision::LocalSectionOnly
    );

    let border = full_mask();
    assert_eq!(
        SectionOcclusionMask::classify_section_mutation(
            &border,
            0,
            15,
            0,
            BlockRenderTraits::FULL_OPAQUE_CUBE,
        ),
        RemeshDecision::SectionAndBorderNeighbors(FACE_NEG_X | FACE_POS_Y | FACE_NEG_Z)
    );
}

#[test]
fn protruding_mod_model_is_never_elided_even_when_buried() {
    let mut mask = full_mask();
    let protruding =
        BlockRenderTraits::FULL_OPAQUE_CUBE.union(BlockRenderTraits::PROTRUDING_BOUNDS);
    mask.set_render_traits(8, 8, 8, protruding);
    assert!(!mask.can_elide_remesh_traits(
        8,
        8,
        8,
        protruding,
        BlockRenderTraits::FULL_OPAQUE_CUBE,
    ));
    assert_eq!(
        SectionOcclusionMask::classify_section_mutation(
            &mask,
            8,
            8,
            8,
            BlockRenderTraits::FULL_OPAQUE_CUBE,
        ),
        RemeshDecision::LocalSectionOnly,
        "Create-style oversized visuals must fail closed"
    );
}

#[test]
fn neighbor_dependent_visual_ctm_blocks_elision() {
    let mut mask = full_mask();
    let ctm =
        BlockRenderTraits::FULL_OPAQUE_CUBE.union(BlockRenderTraits::NEIGHBOR_DEPENDENT_VISUAL);
    mask.set_render_traits(7, 8, 8, ctm);
    assert!(mask.is_strict_occluder_at(7, 8, 8));
    assert!(mask.is_neighbor_sensitive_at(7, 8, 8));
    assert_eq!(
        SectionOcclusionMask::classify_section_mutation(
            &mask,
            8,
            8,
            8,
            BlockRenderTraits::FULL_OPAQUE_CUBE,
        ),
        RemeshDecision::LocalSectionOnly
    );
}

#[test]
fn corner_ctm_mode_rejects_diagonal_neighbor_connected_textures() {
    let mut default_mask = full_mask();
    let ctm =
        BlockRenderTraits::FULL_OPAQUE_CUBE.union(BlockRenderTraits::NEIGHBOR_DEPENDENT_VISUAL);
    default_mask.set_render_traits(7, 7, 7, ctm);
    assert_eq!(
        SectionOcclusionMask::classify_section_mutation(
            &default_mask,
            8,
            8,
            8,
            BlockRenderTraits::FULL_OPAQUE_CUBE,
        ),
        RemeshDecision::Elide,
        "six-face mode deliberately leaves corner CTM policy configurable"
    );

    let mut conservative = default_mask.clone();
    conservative.set_corner_ctm_checks_enabled(true);
    assert_eq!(
        SectionOcclusionMask::classify_section_mutation(
            &conservative,
            8,
            8,
            8,
            BlockRenderTraits::FULL_OPAQUE_CUBE,
        ),
        RemeshDecision::LocalSectionOnly
    );
}

#[test]
fn light_dynamic_and_unknown_renderers_are_never_elided() {
    assert_eq!(
        BlockRenderTraits::default(),
        BlockRenderTraits::UNKNOWN_CONSERVATIVE,
        "uncertified mod blocks must default to the conservative path"
    );
    for flag in [
        BlockRenderTraits::LIGHT_AFFECTING,
        BlockRenderTraits::DYNAMIC_RENDERER,
        BlockRenderTraits::UNKNOWN_CONSERVATIVE,
    ] {
        let mut mask = full_mask();
        let traits = BlockRenderTraits::FULL_OPAQUE_CUBE.union(flag);
        mask.set_render_traits(8, 8, 8, traits);
        let decision = SectionOcclusionMask::classify_section_mutation(&mask, 8, 8, 8, traits);
        assert_ne!(
            decision,
            RemeshDecision::Elide,
            "flag {:?} must fail closed",
            flag
        );
    }
}

#[test]
fn protruding_block_near_section_boundary_expands_neighbor_invalidation() {
    let mut mask = full_mask();
    mask.set_protrusion_radius_cells(2);
    let protruding =
        BlockRenderTraits::FULL_OPAQUE_CUBE.union(BlockRenderTraits::PROTRUDING_BOUNDS);
    mask.set_render_traits(1, 8, 8, protruding);
    assert_eq!(
        SectionOcclusionMask::classify_section_mutation(&mask, 1, 8, 8, protruding),
        RemeshDecision::SectionAndBorderNeighbors(FACE_NEG_X),
        "a two-cell overhang at local x=1 reaches the -X neighbor section"
    );
}

#[test]
fn mesh_decision_connects_to_incremental_dag_early_cutoff() {
    let mut dag = IncrementalDag::new();
    let mesh = dag.add_node(77);
    assert_eq!(
        apply_decision_to_dag(
            &mut dag,
            mesh,
            RemeshDecision::Elide,
            999,
            [None, None, None, None, None, None],
        )
        .expect("elided mesh node"),
        CutoffOutcome::EarlyCutoff
    );
    assert_eq!(dag.digest(mesh), Ok(77));
    assert_eq!(dag.dirty_count(), 0);

    assert_eq!(
        apply_decision_to_dag(
            &mut dag,
            mesh,
            RemeshDecision::LocalSectionOnly,
            123,
            [None, None, None, None, None, None],
        )
        .expect("visible mesh change"),
        CutoffOutcome::Changed
    );
    assert_eq!(dag.digest(mesh), Ok(123));
}

#[test]
fn content_hash_mesh_cache_shares_identical_sections_with_arc_identity() {
    let empty_fp = compute_section_fingerprint(&[0], &[]);
    let mut cache = ContentHashMeshCache::<Vec<u32>>::new(16);
    let (first, first_hit) = cache.get_or_build(empty_fp, || vec![1]);
    let (second, second_hit) = cache.get_or_build(empty_fp, || vec![2]);
    assert!(!first_hit);
    assert!(second_hit);
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(second, Arc::new(vec![1]));

    let mut hits = 0usize;
    let mut shared_empty = None;
    for i in 0..1000usize {
        let fp = if i % 10 == 9 {
            let id = (i / 10) as u32 + 1;
            compute_section_fingerprint(&[id], &[id as u8])
        } else {
            empty_fp
        };
        let (mesh, hit) = cache.get_or_build(fp, || vec![42]);
        if fp == empty_fp {
            if let Some(previous) = &shared_empty {
                assert!(Arc::ptr_eq(previous, &mesh));
            } else {
                shared_empty = Some(Arc::clone(&mesh));
            }
        }
        hits += usize::from(hit);
    }
    assert!(cache.dedup_ratio() >= 0.85, "ratio {}", cache.dedup_ratio());
    assert!(hits as f64 / 1000.0 >= 0.85);
    assert!(cache.len() <= 16);
}

#[test]
fn distinct_section_layouts_have_distinct_content_fingerprints() {
    let stone = compute_section_fingerprint(&[1], &[0, 0, 0, 0]);
    let deepslate = compute_section_fingerprint(&[2], &[0, 0, 0, 0]);
    let shifted = compute_section_fingerprint(&[1], &[1, 0, 0, 0]);
    assert_ne!(stone, deepslate);
    assert_ne!(stone, shifted);
    assert_ne!(deepslate, shifted);
}

#[test]
fn frame_pacing_ring_respects_budget_and_prioritizes_near_chunks() {
    let mut ring = FramePacingUploadRing::new();
    let far = upload_key(100);
    let near = upload_key(1);
    let mid = upload_key(20);
    ring.push(10, 100, far, "far");
    ring.push(100, 300, near, "near");
    ring.push(50, 200, mid, "mid");

    let mut out = Vec::new();
    let drained = ring.drain_frame_budget(450, &mut out);
    assert_eq!(drained, 300);
    assert_eq!(
        out.iter().map(|(_, payload)| *payload).collect::<Vec<_>>(),
        vec!["near"],
        "strict priority must not let a far chunk overtake the mid-priority head"
    );
    assert_eq!(ring.len(), 2);
    assert_eq!(ring.pending_bytes(), 300);

    out.clear();
    assert_eq!(ring.drain_frame_budget(450, &mut out), 300);
    assert_eq!(
        out.iter().map(|(_, payload)| *payload).collect::<Vec<_>>(),
        vec!["mid", "far"]
    );
    assert!(ring.is_empty());
}

#[test]
fn frame_pacing_ring_deduplicates_stale_keys_and_never_starves_oversized() {
    let mut ring = FramePacingUploadRing::new();
    let key = upload_key(7);
    ring.push(1, 100, key, "stale");
    ring.push(9, 50, key, "fresh");
    assert_eq!(ring.len(), 1);
    assert_eq!(ring.pending_bytes(), 50);
    let mut out = Vec::new();
    assert_eq!(ring.drain_frame_budget(100, &mut out), 50);
    assert_eq!(out, vec![(key, "fresh")]);

    ring.push(100, 1_000_000, key, "oversized");
    ring.push(1, 8, upload_key(8), "small");
    out.clear();
    assert_eq!(ring.drain_frame_budget(16, &mut out), 1_000_000);
    assert_eq!(
        out,
        vec![(key, "oversized")],
        "the highest-priority oversized upload must be admitted alone"
    );
    out.clear();
    assert_eq!(ring.drain_frame_budget(16, &mut out), 8);
    assert_eq!(out, vec![(upload_key(8), "small")]);
    assert!(ring.is_empty());
}

#[test]
fn dst_rng_is_exactly_reproducible() {
    let mut a = DstRng::new(0x5eed);
    let mut b = DstRng::new(0x5eed);
    let seq_a: Vec<u64> = (0..32).map(|_| a.next_u64()).collect();
    let seq_b: Vec<u64> = (0..32).map(|_| b.next_u64()).collect();
    assert_eq!(seq_a, seq_b);
    assert_eq!(a.state(), b.state());
}

#[test]
fn dst_snapshot_roundtrip_is_merkle_identical_before_corruption() {
    let mut pre = DstSimulation::new();
    pre.apply_op(DstOp::MutateBlock {
        dim: DimIndex::OVERWORLD,
        pos: BlockPos::new(1, 2, 3),
        block_id: 7,
    })
    .expect("mutation");
    pre.validate_invariants().expect("valid pre-crash world");
    let snapshot = pre.snapshot_bytes();
    let mut reloaded = DstSimulation::from_snapshot(&snapshot).expect("snapshot parses");
    verify_merkle_restart(&mut pre, &mut reloaded, 99).expect("roundtrip root equality");
}

#[test]
fn dst_negative_item_dupe_is_immediately_caught() {
    let mut simulation = DstSimulation::new();
    assert!(simulation.inject_item_dupe(3, 1));
    let error = simulation
        .validate_invariants()
        .expect_err("one injected item must violate conservation");
    assert_eq!(
        error,
        DstInvariantViolation::ItemConservation {
            tick: 0,
            expected: INITIAL_TOTAL_ITEMS,
            actual: INITIAL_TOTAL_ITEMS + 1,
        }
    );
}

#[test]
fn dst_negative_one_block_restart_corruption_pinpoints_exact_merkle_key() {
    let pos = BlockPos::new(1, 2, 3);
    let mut pre = DstSimulation::new();
    pre.apply_op(DstOp::MutateBlock {
        dim: DimIndex::OVERWORLD,
        pos,
        block_id: 7,
    })
    .expect("mutation");
    let mut reloaded =
        DstSimulation::from_snapshot(&pre.snapshot_bytes()).expect("snapshot parses");
    let corrupted_leaf = reloaded
        .inject_restart_block_corruption()
        .expect("world has a block to corrupt");
    let expected_leaf = chunk_leaf_key(ChunkKey {
        dim: DimIndex::OVERWORLD,
        cx: 0,
        cz: 0,
    });
    assert_eq!(corrupted_leaf, expected_leaf);

    let error = verify_merkle_restart(&mut pre, &mut reloaded, 123)
        .expect_err("one corrupted block must fail restart identity");
    match error {
        DstInvariantViolation::RestartMerkleMismatch {
            divergent_keys,
            pre_root,
            post_root,
            ..
        } => {
            assert_ne!(pre_root, post_root);
            assert_eq!(divergent_keys, vec![expected_leaf]);
        }
        other => panic!("wrong violation: {other:?}"),
    }
}

#[test]
fn dst_campaign_is_deterministic_for_same_seed() {
    let first = run_dst_campaign(0xdecaf, 2_500).expect("first campaign");
    let second = run_dst_campaign(0xdecaf, 2_500).expect("second campaign");
    assert_eq!(first, second);
}

#[test]
fn dst_campaign_multiple_seeds_verifies_all_invariants_under_two_seconds() {
    let started = Instant::now();
    let mut aggregate_ticks = 0usize;
    let mut aggregate_restarts = 0usize;
    let mut aggregate_cross_dim = 0usize;
    let mut aggregate_faults = 0usize;
    for seed in [0x1020_3040_5060_7080, 0xa5a5_5a5a_c3c3_3c3c] {
        let report = run_dst_campaign(seed, 50_000).expect("campaign invariants hold");
        assert_eq!(report.seed, seed);
        assert_eq!(report.ticks_executed, 50_000);
        assert_eq!(report.conservation_checks, 50_000);
        assert_eq!(report.restarts_verified, 100);
        assert_eq!(report.merkle_roots_matched, report.restarts_verified);
        assert!(
            report.cross_dim_ops > 0,
            "portal coverage missing for seed {seed:#x}"
        );
        assert!(
            report.mod_faults_isolated > 0,
            "fault coverage missing for seed {seed:#x}"
        );
        aggregate_ticks += report.ticks_executed;
        aggregate_restarts += report.restarts_verified;
        aggregate_cross_dim += report.cross_dim_ops;
        aggregate_faults += report.mod_faults_isolated;
    }
    assert!(aggregate_ticks >= 50_000);
    assert!(aggregate_restarts >= 200);
    assert!(aggregate_cross_dim > 0);
    assert!(aggregate_faults > 0);
    let budget_secs = if cfg!(debug_assertions) { 20.0 } else { 2.0 };
    assert!(
        started.elapsed().as_secs_f64() < budget_secs,
        "DST campaign took {:?} (budget {budget_secs}s)",
        started.elapsed()
    );
}
