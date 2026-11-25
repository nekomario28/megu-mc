// Cold-start stage profiler.
//
// `MEGU_STARTUP_PROFILE=1` prints one `[startup]` line per boot stage to
// stderr: cumulative milliseconds since `StartupProfiler::new()` (created as
// the first statement of `main`, so `+0.000` marks entry after exec/loader
// work the process itself cannot see), the delta from the previous mark, and
// the stage name. `startup_profile_parallel` reports a stage that ran on a
// helper thread beside the main boot path, so overlapped work still shows in
// the breakdown. With the flag unset both entry points cost one `OnceLock`
// read; the marks only ever run on the single-threaded boot path, so the
// profiler itself is plain state, no lock.

/// Whether the boot-stage breakdown is armed. Read once — the flag is an
/// operator diagnostic set before exec, never toggled at runtime.
fn startup_profile_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| env_flag("MEGU_STARTUP_PROFILE"))
}

/// One line of the `MEGU_STARTUP_PROFILE` report for a stage that finished on
/// a helper thread: its own wall time, not a position on the main timeline.
fn startup_profile_parallel(stage: &str, elapsed: Duration) {
    if !startup_profile_enabled() {
        return;
    }
    eprintln!(
        "[startup]   parallel {stage}: {:>8.3} ms",
        elapsed.as_secs_f64() * 1_000.0
    );
}

struct StartupProfiler {
    t0: Instant,
    last: Instant,
}

impl StartupProfiler {
    fn new() -> StartupProfiler {
        let now = Instant::now();
        StartupProfiler { t0: now, last: now }
    }

    /// Emit `[startup] <total> ms (+<delta>) <stage>` for the point the boot
    /// path just reached; a no-op unless `MEGU_STARTUP_PROFILE` is armed.
    fn mark(&mut self, stage: &str) {
        if !startup_profile_enabled() {
            return;
        }
        let now = Instant::now();
        eprintln!(
            "[startup] {:>9.3} ms (+{:>7.3}) {}",
            self.t0.elapsed().as_secs_f64() * 1_000.0,
            (now - self.last).as_secs_f64() * 1_000.0,
            stage
        );
        self.last = now;
    }
}

#[cfg(test)]
mod startup_profile_tests {
    use super::*;

    /// A disabled profiler must be constructible, markable and droppable
    /// without touching stderr — the marks only gate on the env flag, so a
    /// plain mark() call exercises the whole code path minus the print.
    #[test]
    fn profiler_marks_do_not_panic() {
        let mut prof = StartupProfiler::new();
        prof.mark("cli-flags");
        prof.mark("tcp-bind");
        prof.mark("ready");
    }

    /// The parallel-stage line takes any duration, including zero.
    #[test]
    fn parallel_report_accepts_zero() {
        startup_profile_parallel("block-registry", Duration::ZERO);
        startup_profile_parallel("online-auth", Duration::from_millis(3));
    }
}
