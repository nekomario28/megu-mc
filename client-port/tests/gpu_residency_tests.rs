// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client_mesh::{GpuResidencyManager, ResidencyBudget, SectionCoord, SectionResidencyState};

fn budget() -> ResidencyBudget {
    ResidencyBudget {
        hard_vram_budget_bytes: 1_000,
        high_watermark_bytes: 700,
        low_watermark_bytes: 400,
        max_rebakes_per_frame: 2,
        max_evictions_per_frame: 8,
        eviction_cooldown_frames: 5,
    }
}

fn resident(manager: &mut GpuResidencyManager, x: i32, bytes: u64, frame: u64) {
    manager.register_resident(SectionCoord(x, 0, 0), bytes, 1, frame);
}

#[test]
fn hysteresis_evicts_farthest_non_visible_down_to_low_watermark() {
    let mut b = budget();
    b.high_watermark_bytes = 800;
    let mut manager = GpuResidencyManager::new(b);
    resident(&mut manager, 1, 200, 0);
    resident(&mut manager, 4, 200, 0);
    resident(&mut manager, 8, 200, 0);
    resident(&mut manager, 12, 200, 0);
    assert!(manager
        .plan_frame(SectionCoord(0, 0, 0), &[], 1)
        .evict_coords
        .is_empty());
    assert!(manager
        .plan_frame(SectionCoord(0, 0, 0), &[], 2)
        .evict_coords
        .is_empty());
    resident(&mut manager, 16, 200, 0);
    let plan = manager.plan_frame(SectionCoord(0, 0, 0), &[], 3);
    assert_eq!(
        plan.evict_coords,
        [
            SectionCoord(16, 0, 0),
            SectionCoord(12, 0, 0),
            SectionCoord(8, 0, 0)
        ]
    );
    assert_eq!(manager.stats().resident_vram_bytes, 400);
    assert!(manager
        .plan_frame(SectionCoord(0, 0, 0), &[], 4)
        .evict_coords
        .is_empty());
}

#[test]
fn eviction_cooldown_prevents_immediate_rebake_and_re_evict_cycle() {
    let mut manager = GpuResidencyManager::new(budget());
    resident(&mut manager, 20, 800, 0);
    let evicted = manager
        .plan_frame(SectionCoord(0, 0, 0), &[], 1)
        .evict_coords;
    assert_eq!(evicted, [SectionCoord(20, 0, 0)]);
    let queue = manager.plan_frame(SectionCoord(20, 0, 0), &[SectionCoord(20, 0, 0)], 2);
    assert!(queue.rebake_coords.is_empty());
    assert_eq!(manager.stats().thrash_prevented_count, 1);
    let queue = manager.plan_frame(SectionCoord(20, 0, 0), &[SectionCoord(20, 0, 0)], 6);
    assert_eq!(queue.rebake_coords, [SectionCoord(20, 0, 0)]);
    manager.register_resident(SectionCoord(20, 0, 0), 800, 1, 6);
    assert!(manager
        .plan_frame(SectionCoord(0, 0, 0), &[], 7)
        .evict_coords
        .is_empty());
    assert_eq!(manager.stats().resident_vram_bytes, 800);
    assert!(manager
        .plan_frame(SectionCoord(0, 0, 0), &[], 10)
        .evict_coords
        .is_empty());
    assert_eq!(
        manager
            .plan_frame(SectionCoord(0, 0, 0), &[], 11)
            .evict_coords,
        [SectionCoord(20, 0, 0)]
    );
}

#[test]
fn visible_evicted_sections_rebake_nearest_first_with_per_frame_cap() {
    let mut manager = GpuResidencyManager::new(ResidencyBudget {
        high_watermark_bytes: 0,
        low_watermark_bytes: 0,
        eviction_cooldown_frames: 0,
        ..budget()
    });
    for x in [2, 3, 4] {
        manager.sections_for_test_register_evicted(SectionCoord(x, 0, 0), 7, 100, 0);
    }
    let visible = [
        SectionCoord(4, 0, 0),
        SectionCoord(2, 0, 0),
        SectionCoord(3, 0, 0),
    ];
    let plan = manager.plan_frame(SectionCoord(0, 0, 0), &visible, 1);
    assert_eq!(
        plan.rebake_coords,
        [SectionCoord(2, 0, 0), SectionCoord(3, 0, 0)]
    );
    assert_eq!(manager.stats().rebake_queued_count, 2);
    manager.register_resident(SectionCoord(2, 0, 0), 100, 7, 1);
    assert_eq!(manager.stats().resident_vram_bytes, 100);
    assert_eq!(manager.stats().total_rebakes, 1);
}

#[test]
fn dirty_while_evicted_waits_until_visible_and_uses_latest_revision() {
    let mut manager = GpuResidencyManager::new(ResidencyBudget {
        high_watermark_bytes: 0,
        low_watermark_bytes: 0,
        eviction_cooldown_frames: 0,
        ..budget()
    });
    manager.sections_for_test_register_evicted(SectionCoord(5, 0, 0), 1, 100, 0);
    manager.mark_section_dirty(SectionCoord(5, 0, 0), 9);
    assert!(manager
        .plan_frame(SectionCoord(0, 0, 0), &[], 1)
        .rebake_coords
        .is_empty());
    assert_eq!(
        manager.section_state(SectionCoord(5, 0, 0)),
        Some(SectionResidencyState::AliveButEvicted {
            world_revision: 9,
            evicted_at_frame: 1,
            estimated_gpu_bytes: 100,
        })
    );
    assert_eq!(
        manager
            .plan_frame(SectionCoord(5, 0, 0), &[SectionCoord(5, 0, 0)], 2)
            .rebake_coords,
        [SectionCoord(5, 0, 0)]
    );
    assert_eq!(
        manager.section_state(SectionCoord(5, 0, 0)),
        Some(SectionResidencyState::RebakeQueued {
            world_revision: 9,
            priority_score: u64::MAX,
        })
    );
}

#[test]
fn hard_budget_bounds_five_hundred_section_fly_through() {
    let mut b = budget();
    b.hard_vram_budget_bytes = 64 * 1024 * 1024;
    b.high_watermark_bytes = 48 * 1024 * 1024;
    b.low_watermark_bytes = 40 * 1024 * 1024;
    b.max_evictions_per_frame = 64;
    let mut manager = GpuResidencyManager::new(b);
    for frame in 0..500u64 {
        let coord = SectionCoord(frame as i32, 0, 0);
        manager.register_resident(coord, 1024 * 1024, frame, frame);
        let visible = [coord];
        manager.plan_frame(coord, &visible, frame);
        assert!(manager.stats().resident_vram_bytes <= b.hard_vram_budget_bytes);
    }
    assert!(manager.stats().resident_vram_bytes <= b.low_watermark_bytes + 1024 * 1024);
}

#[test]
fn unload_accounting_covers_resident_evicted_and_queued_states() {
    let mut manager = GpuResidencyManager::new(ResidencyBudget {
        eviction_cooldown_frames: 0,
        ..budget()
    });
    resident(&mut manager, 0, 100, 0);
    resident(&mut manager, 10, 800, 0);
    assert_eq!(
        manager
            .plan_frame(SectionCoord(0, 0, 0), &[], 1)
            .evict_coords,
        [SectionCoord(10, 0, 0)]
    );
    let queued = SectionCoord(10, 0, 0);
    manager.plan_frame(queued, &[queued], 2);
    assert!(matches!(
        manager.section_state(queued),
        Some(SectionResidencyState::RebakeQueued { .. })
    ));
    assert!(manager.remove_unloaded_section(SectionCoord(0, 0, 0)));
    assert!(manager.remove_unloaded_section(queued));
    assert!(!manager.remove_unloaded_section(queued));
    let stats = manager.stats();
    assert_eq!(stats.resident_count, 0);
    assert_eq!(stats.evicted_count, 0);
    assert_eq!(stats.rebake_queued_count, 0);
    assert_eq!(stats.resident_vram_bytes, 0);
    assert_eq!(stats.total_evictions, 1);
}

trait TestRegisterEvicted {
    fn sections_for_test_register_evicted(
        &mut self,
        coord: SectionCoord,
        revision: u64,
        estimate: u64,
        frame: u64,
    );
}

impl TestRegisterEvicted for GpuResidencyManager {
    fn sections_for_test_register_evicted(
        &mut self,
        coord: SectionCoord,
        revision: u64,
        estimate: u64,
        frame: u64,
    ) {
        self.register_resident(coord, estimate, revision, frame);
        let plan = self.plan_frame(SectionCoord(i32::MIN, 0, 0), &[], frame.saturating_add(1));
        assert!(plan.evict_coords.contains(&coord));
    }
}
