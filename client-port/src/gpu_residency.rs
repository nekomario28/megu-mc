// SPDX-License-Identifier: LGPL-3.0-or-later

//! Deterministic bookkeeping for GPU section residency and on-demand rebakes.
//!
//! This manager produces eviction and rebake work plans. The render loop must
//! apply `evict_coords` to GPU allocations and provide rebaked meshes back via
//! `register_resident`; CPU section data remains owned by the world cache.

use std::collections::{BTreeMap, BTreeSet};

use crate::SectionCoord;

/// Lifecycle state for the GPU copy of a loaded world section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SectionResidencyState {
    /// A GPU mesh is allocated for this section.
    Resident {
        gpu_bytes: u64,
        last_visible_frame: u64,
        mesh_revision: u64,
    },
    /// The world section remains loaded in client memory, but has no GPU mesh.
    AliveButEvicted {
        world_revision: u64,
        evicted_at_frame: u64,
        estimated_gpu_bytes: u64,
    },
    /// A visible evicted section is queued for mesh rebuild and upload.
    RebakeQueued {
        world_revision: u64,
        priority_score: u64,
    },
}

/// VRAM and per-frame work limits for [`GpuResidencyManager`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResidencyBudget {
    pub hard_vram_budget_bytes: u64,
    pub high_watermark_bytes: u64,
    pub low_watermark_bytes: u64,
    pub max_rebakes_per_frame: usize,
    pub max_evictions_per_frame: usize,
    pub eviction_cooldown_frames: u64,
}

impl Default for ResidencyBudget {
    fn default() -> Self {
        Self {
            hard_vram_budget_bytes: 512 * 1024 * 1024,
            high_watermark_bytes: 448 * 1024 * 1024,
            low_watermark_bytes: 384 * 1024 * 1024,
            max_rebakes_per_frame: 4,
            max_evictions_per_frame: 16,
            eviction_cooldown_frames: 30,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    state: SectionResidencyState,
    last_evicted_frame: Option<u64>,
}

/// Actions for the renderer and mesh scheduler to apply during one frame.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResidencyFramePlan {
    /// GPU allocations to release; sections remain loaded on the CPU.
    pub evict_coords: Vec<SectionCoord>,
    /// Visible sections to rebuild, nearest first.
    pub rebake_coords: Vec<SectionCoord>,
}

/// Snapshot of tracked section counts and lifetime work totals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResidencyStats {
    pub resident_count: usize,
    pub evicted_count: usize,
    pub rebake_queued_count: usize,
    pub resident_vram_bytes: u64,
    pub total_evictions: u64,
    pub total_rebakes: u64,
    pub thrash_prevented_count: u64,
}

/// Stable, allocation-disciplined GPU residency state and scheduler.
#[derive(Clone, Debug)]
pub struct GpuResidencyManager {
    budget: ResidencyBudget,
    sections: BTreeMap<SectionCoord, Entry>,
    resident_vram_bytes: u64,
    total_evictions: u64,
    total_rebakes: u64,
    thrash_prevented_count: u64,
}

impl GpuResidencyManager {
    pub fn new(budget: ResidencyBudget) -> Self {
        Self {
            budget,
            sections: BTreeMap::new(),
            resident_vram_bytes: 0,
            total_evictions: 0,
            total_rebakes: 0,
            thrash_prevented_count: 0,
        }
    }

    pub fn budget(&self) -> ResidencyBudget {
        self.budget
    }

    pub fn section_state(&self, coord: SectionCoord) -> Option<SectionResidencyState> {
        self.sections.get(&coord).map(|entry| entry.state)
    }

    /// Register an allocated GPU mesh and replace any previous lifecycle state.
    pub fn register_resident(
        &mut self,
        coord: SectionCoord,
        gpu_bytes: u64,
        world_revision: u64,
        frame: u64,
    ) {
        let mut last_evicted_frame = None;
        if let Some(previous) = self.sections.remove(&coord) {
            self.subtract_resident(previous.state);
            if !matches!(previous.state, SectionResidencyState::Resident { .. }) {
                last_evicted_frame = Some(frame);
            } else {
                last_evicted_frame = previous.last_evicted_frame;
            }
            if matches!(previous.state, SectionResidencyState::RebakeQueued { .. }) {
                self.total_rebakes = self.total_rebakes.saturating_add(1);
            }
        }
        self.resident_vram_bytes = self.resident_vram_bytes.saturating_add(gpu_bytes);
        self.sections.insert(
            coord,
            Entry {
                state: SectionResidencyState::Resident {
                    gpu_bytes,
                    last_visible_frame: frame,
                    mesh_revision: world_revision,
                },
                last_evicted_frame,
            },
        );
    }

    /// Update the authoritative world revision without rebuilding while hidden.
    pub fn mark_section_dirty(&mut self, coord: SectionCoord, new_world_revision: u64) {
        if let Some(entry) = self.sections.get_mut(&coord) {
            match entry.state {
                SectionResidencyState::Resident {
                    gpu_bytes,
                    last_visible_frame,
                    ..
                } => {
                    entry.state = SectionResidencyState::Resident {
                        gpu_bytes,
                        last_visible_frame,
                        mesh_revision: new_world_revision,
                    };
                }
                SectionResidencyState::AliveButEvicted {
                    evicted_at_frame,
                    estimated_gpu_bytes,
                    ..
                } => {
                    entry.state = SectionResidencyState::AliveButEvicted {
                        world_revision: new_world_revision,
                        evicted_at_frame,
                        estimated_gpu_bytes,
                    };
                }
                SectionResidencyState::RebakeQueued { priority_score, .. } => {
                    entry.state = SectionResidencyState::RebakeQueued {
                        world_revision: new_world_revision,
                        priority_score,
                    };
                }
            }
        }
    }

    /// Forget a section unloaded by the server and release its accounted VRAM.
    pub fn remove_unloaded_section(&mut self, coord: SectionCoord) -> bool {
        let Some(entry) = self.sections.remove(&coord) else {
            return false;
        };
        self.subtract_resident(entry.state);
        true
    }

    /// Plan visible re-bakes and low-priority evictions for one frame.
    pub fn plan_frame(
        &mut self,
        camera_section: SectionCoord,
        visible_sections: &[SectionCoord],
        frame: u64,
    ) -> ResidencyFramePlan {
        let visible: BTreeSet<_> = visible_sections.iter().copied().collect();
        for coord in &visible {
            if let Some(entry) = self.sections.get_mut(coord) {
                if let SectionResidencyState::Resident {
                    gpu_bytes,
                    mesh_revision,
                    ..
                } = entry.state
                {
                    entry.state = SectionResidencyState::Resident {
                        gpu_bytes,
                        last_visible_frame: frame,
                        mesh_revision,
                    };
                }
            }
        }

        let mut plan = ResidencyFramePlan::default();
        let must_evict = self.resident_vram_bytes > self.budget.high_watermark_bytes;
        if must_evict || self.resident_vram_bytes > self.budget.hard_vram_budget_bytes {
            let target = if self.resident_vram_bytes > self.budget.hard_vram_budget_bytes {
                self.budget.hard_vram_budget_bytes
            } else {
                self.budget.low_watermark_bytes
            };
            let mut candidates: Vec<_> = self
                .sections
                .iter()
                .filter_map(|(&coord, entry)| {
                    let SectionResidencyState::Resident {
                        gpu_bytes,
                        last_visible_frame,
                        ..
                    } = entry.state
                    else {
                        return None;
                    };
                    if self.resident_vram_bytes <= self.budget.hard_vram_budget_bytes
                        && entry.last_evicted_frame.is_some_and(|evicted| {
                            frame.saturating_sub(evicted) < self.budget.eviction_cooldown_frames
                        })
                    {
                        return None;
                    }
                    Some((
                        coord,
                        gpu_bytes,
                        last_visible_frame,
                        visible.contains(&coord),
                        distance_sq(coord, camera_section),
                    ))
                })
                .collect();
            candidates.sort_by_key(|(coord, _, last_visible, is_visible, distance)| {
                (
                    *is_visible,
                    std::cmp::Reverse(*distance),
                    *last_visible,
                    *coord,
                )
            });
            for (coord, gpu_bytes, _, is_visible, _) in candidates {
                if self.resident_vram_bytes <= target
                    || plan.evict_coords.len() >= self.budget.max_evictions_per_frame
                {
                    break;
                }
                if is_visible && self.resident_vram_bytes <= self.budget.hard_vram_budget_bytes {
                    continue;
                }
                let Some(entry) = self.sections.get_mut(&coord) else {
                    continue;
                };
                entry.state = SectionResidencyState::AliveButEvicted {
                    world_revision: match entry.state {
                        SectionResidencyState::Resident { mesh_revision, .. } => mesh_revision,
                        _ => unreachable!(),
                    },
                    evicted_at_frame: frame,
                    estimated_gpu_bytes: gpu_bytes,
                };
                entry.last_evicted_frame = Some(frame);
                self.resident_vram_bytes = self.resident_vram_bytes.saturating_sub(gpu_bytes);
                self.total_evictions = self.total_evictions.saturating_add(1);
                plan.evict_coords.push(coord);
            }
        }

        let headroom = self
            .budget
            .hard_vram_budget_bytes
            .saturating_sub(self.resident_vram_bytes);
        let mut requests: Vec<_> = self
            .sections
            .iter()
            .filter_map(|(&coord, entry)| {
                let SectionResidencyState::AliveButEvicted {
                    world_revision,
                    evicted_at_frame,
                    estimated_gpu_bytes,
                } = entry.state
                else {
                    return None;
                };
                if !visible.contains(&coord) {
                    return None;
                }
                if frame.saturating_sub(evicted_at_frame) < self.budget.eviction_cooldown_frames {
                    self.thrash_prevented_count = self.thrash_prevented_count.saturating_add(1);
                    return None;
                }
                Some((
                    distance_sq(coord, camera_section),
                    coord,
                    world_revision,
                    estimated_gpu_bytes,
                ))
            })
            .collect();
        requests.sort_by_key(|(distance, coord, _, _)| (*distance, *coord));
        let mut reserved = 0u64;
        for (_, coord, world_revision, estimate) in requests {
            if plan.rebake_coords.len() >= self.budget.max_rebakes_per_frame {
                break;
            }
            if reserved.saturating_add(estimate) > headroom {
                self.thrash_prevented_count = self.thrash_prevented_count.saturating_add(1);
                continue;
            }
            reserved = reserved.saturating_add(estimate);
            if let Some(entry) = self.sections.get_mut(&coord) {
                entry.state = SectionResidencyState::RebakeQueued {
                    world_revision,
                    priority_score: u64::MAX.saturating_sub(distance_sq(coord, camera_section)),
                };
            }
            plan.rebake_coords.push(coord);
        }
        plan
    }

    pub fn stats(&self) -> ResidencyStats {
        let mut stats = ResidencyStats {
            resident_vram_bytes: self.resident_vram_bytes,
            total_evictions: self.total_evictions,
            total_rebakes: self.total_rebakes,
            thrash_prevented_count: self.thrash_prevented_count,
            ..ResidencyStats::default()
        };
        for entry in self.sections.values() {
            match entry.state {
                SectionResidencyState::Resident { .. } => stats.resident_count += 1,
                SectionResidencyState::AliveButEvicted { .. } => stats.evicted_count += 1,
                SectionResidencyState::RebakeQueued { .. } => stats.rebake_queued_count += 1,
            }
        }
        stats
    }

    fn subtract_resident(&mut self, state: SectionResidencyState) {
        if let SectionResidencyState::Resident { gpu_bytes, .. } = state {
            self.resident_vram_bytes = self.resident_vram_bytes.saturating_sub(gpu_bytes);
        }
    }
}

fn distance_sq(a: SectionCoord, b: SectionCoord) -> u64 {
    let axis = |x: i32, y: i32| (i64::from(x) - i64::from(y)).unsigned_abs();
    let [x, y, z] = [axis(a.0, b.0), axis(a.1, b.1), axis(a.2, b.2)];
    x.saturating_mul(x)
        .saturating_add(y.saturating_mul(y))
        .saturating_add(z.saturating_mul(z))
}
