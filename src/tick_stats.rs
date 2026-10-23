//! Measured tick statistics: how long the tick body took, and whether it stayed
//! inside the budget the server promises (20 TPS).
//!
//! The point is the claim in PLAN G3 ("20TPS at the representative load"): a
//! number nothing measures drifts, and until now the only tick timing lived in a
//! test-only profiler. This is the working server's own record, emitted once per
//! window so a load comparison can read it from the log.
//!
//! The budget is passed in rather than declared here: the tick period has one
//! owner (the time contract in `main`), and a second copy of 50 ms is how two
//! owners of the same concept start disagreeing.

use std::time::{Duration, Instant};

/// Ticks per report. 100 ticks is five seconds at 20 TPS: long enough that the
/// line itself is not a per-tick cost, short enough to see a burst.
pub(crate) const SAMPLE_TICKS: u32 = 100;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TickReport {
    pub(crate) observed_tps: f64,
    pub(crate) mean_work_ms: f64,
    pub(crate) max_work_ms: f64,
    pub(crate) overruns: u32,
}

pub(crate) struct TickStats {
    budget: Duration,
    window_start: Instant,
    count: u32,
    work_total: Duration,
    work_max: Duration,
    overruns: u32,
}

impl TickStats {
    pub(crate) fn new(now: Instant, budget: Duration) -> Self {
        Self {
            budget,
            window_start: now,
            count: 0,
            work_total: Duration::ZERO,
            work_max: Duration::ZERO,
            overruns: 0,
        }
    }

    /// Record one tick's work. Returns a report every `SAMPLE_TICKS` ticks.
    pub(crate) fn record(&mut self, now: Instant, work: Duration) -> Option<TickReport> {
        self.count += 1;
        self.work_total = self.work_total.saturating_add(work);
        self.work_max = self.work_max.max(work);
        if work > self.budget {
            self.overruns += 1;
        }
        if self.count < SAMPLE_TICKS {
            return None;
        }
        let elapsed = now
            .saturating_duration_since(self.window_start)
            .as_secs_f64();
        let report = TickReport {
            // The tick count over the elapsed wall time, which is the TPS a
            // client sees - not to be confused with the requested cadence.
            observed_tps: if elapsed > 0.0 {
                self.count as f64 / elapsed
            } else {
                0.0
            },
            mean_work_ms: self.work_total.as_secs_f64() * 1000.0 / self.count as f64,
            max_work_ms: self.work_max.as_secs_f64() * 1000.0,
            overruns: self.overruns,
        };
        *self = Self::new(now, self.budget);
        Some(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget() -> Duration {
        Duration::from_millis(50)
    }

    #[test]
    fn a_window_reports_at_100_ticks_and_resets() {
        let start = Instant::now();
        let mut stats = TickStats::new(start, budget());
        for i in 0..SAMPLE_TICKS {
            let report = stats.record(
                start + Duration::from_millis(50 * i as u64),
                Duration::from_millis(1),
            );
            if i + 1 < SAMPLE_TICKS {
                assert!(report.is_none(), "reported early at tick {i}");
            } else {
                let r = report.expect("the hundredth tick reports");
                assert_eq!(r.overruns, 0);
                assert_eq!(r.mean_work_ms, 1.0);
                assert_eq!(r.max_work_ms, 1.0);
                // 100 ticks over the 4950 ms the timestamps above span
                assert!((r.observed_tps - 100.0 / 4.95).abs() < 1e-6, "{r:?}");
            }
        }
        // the window has been reset: the next tick does not report
        assert!(stats.record(start, Duration::from_millis(1)).is_none());
    }

    #[test]
    fn work_above_the_budget_counts_as_an_overrun() {
        let start = Instant::now();
        let mut stats = TickStats::new(start, budget());
        let mut reports = None;
        for i in 0..SAMPLE_TICKS {
            let work = if i % 2 == 0 {
                Duration::from_millis(60)
            } else {
                Duration::from_millis(10)
            };
            reports = stats.record(start + Duration::from_millis(50 * i as u64), work);
        }
        let r = reports.expect("reports at the end of the window");
        assert_eq!(r.overruns, 50, "half the ticks were over budget");
        assert_eq!(r.max_work_ms, 60.0);
        assert_eq!(r.mean_work_ms, 35.0);
    }

    #[test]
    fn exactly_the_budget_is_not_an_overrun() {
        let start = Instant::now();
        let mut stats = TickStats::new(start, budget());
        let mut reports = None;
        for i in 0..SAMPLE_TICKS {
            reports = stats.record(start + Duration::from_millis(50 * i as u64), budget());
        }
        assert_eq!(reports.expect("reports").overruns, 0);
    }
}
