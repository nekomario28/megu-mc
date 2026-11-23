// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Shared statistical helpers for core metrics.

/// Nearest-rank percentile, sorting with IEEE total ordering.
pub(crate) fn nearest_rank_percentile(samples: &[f64], percentile: f64) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let p = if percentile.is_finite() {
        percentile.clamp(0.0, 100.0)
    } else {
        0.0
    };
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = ((sorted.len() as f64) * (p / 100.0)).ceil() as usize;
    Some(sorted[rank.max(1).min(sorted.len()) - 1])
}
