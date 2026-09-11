//! Optional instrumentation, copied only into private profiling builds.
//! Spans are inclusive elapsed wall time in synchronous handlers, NOT CPU time.
//! They overlap (Dispatch includes Feed; Render includes BuildFrame), include
//! blocking/descheduling and instrumentation overhead, and must not be summed.

use std::{
    path::PathBuf,
    sync::{Mutex, MutexGuard},
    time::Instant,
};

#[derive(Clone, Copy)]
pub enum Kind {
    Feed,
    BuildFrame,
    Render,
    Dispatch,
}

#[derive(Clone, Copy, Default)]
struct Timing {
    calls: u64,
    wall_ns: u128,
}

#[derive(Default)]
struct Stats {
    feed_bytes: u64,
    pane_visits: u64,
    child_checks: u64,
    spans: [Timing; 4],
}

static STATS: Mutex<Stats> = Mutex::new(Stats {
    feed_bytes: 0,
    pane_visits: 0,
    child_checks: 0,
    spans: [Timing {
        calls: 0,
        wall_ns: 0,
    }; 4],
});

fn stats() -> MutexGuard<'static, Stats> {
    // Reporting during panic unwinding must not panic again on a poisoned lock.
    STATS.lock().unwrap_or_else(|error| error.into_inner())
}

pub fn feed_bytes(bytes: usize) {
    let mut stats = stats();
    stats.feed_bytes = stats.feed_bytes.saturating_add(bytes as u64);
}

pub fn pane_visit() {
    let mut stats = stats();
    stats.pane_visits = stats.pane_visits.saturating_add(1);
}

pub fn child_check() {
    let mut stats = stats();
    stats.child_checks = stats.child_checks.saturating_add(1);
}

#[must_use = "retain the guard until the synchronous handler returns"]
pub struct Span {
    kind: Kind,
    started: Instant,
}

impl Span {
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            started: Instant::now(),
        }
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed().as_nanos();
        let mut stats = stats();
        let timing = &mut stats.spans[self.kind as usize];
        timing.calls = timing.calls.saturating_add(1);
        timing.wall_ns = timing.wall_ns.saturating_add(elapsed);
    }
}

#[must_use = "retain the report guard until main returns"]
pub struct Report {
    path: Option<PathBuf>,
    started: Instant,
}

impl Report {
    pub fn new() -> Self {
        Self {
            path: std::env::var_os("TPANE_PROFILE_FILE").map(PathBuf::from),
            started: Instant::now(),
        }
    }
}

impl Drop for Report {
    fn drop(&mut self) {
        let Some(path) = &self.path else { return };
        let lifetime_ns = self.started.elapsed().as_nanos();
        let stats = stats();
        let mut spans = String::new();
        for (index, name) in ["Feed", "BuildFrame", "Render", "Dispatch"]
            .iter()
            .enumerate()
        {
            if index != 0 {
                spans.push(',');
            }
            let timing = stats.spans[index];
            spans.push_str(&format!(
                "\n    \"{name}\": {{\"calls\": {}, \"inclusive_wall_ns\": {}}}",
                timing.calls, timing.wall_ns,
            ));
        }
        // All interpolated values are integers or fixed identifiers, so no
        // dependency or user-controlled JSON string escaping is needed.
        let json = format!(
            concat!(
                "{{\n  \"schema_version\": 1,\n",
                "  \"profiled\": true,\n  \"performance_comparison_valid\": false,\n",
                "  \"measurement\": \"inclusive elapsed wall time, not CPU time\",\n",
                "  \"scope\": \"main entry to return, including startup, workload, drain and shutdown; not just the five-second busy window\",\n",
                "  \"limitations\": \"synchronous handlers only; nested spans overlap and must not be summed; includes blocking, descheduling and profiler overhead; excludes uninstrumented workers and async waits\",\n",
                "  \"counter_semantics\": {{\"feedBytes\": \"bytes offered to Engine.feed, including failed calls\", ",
                "\"paneVisits\": \"sync Pane.poll calls or native Pane.output calls, not unique panes\", ",
                "\"childChecks\": \"Session.try_wait calls including cached status; excludes Drop direct child.try_wait\"}},\n",
                "  \"lifetime_wall_ns\": {},\n",
                "  \"counts\": {{\"feedBytes\": {}, \"paneVisits\": {}, \"childChecks\": {}}},\n",
                "  \"spans\": {{{}\n  }}\n}}\n",
            ),
            lifetime_ns, stats.feed_bytes, stats.pane_visits, stats.child_checks, spans,
        );
        drop(stats);
        // SIGKILL / abort / process::exit cannot run this guard. The driver
        // requires a fresh, parseable report and treats its absence as failure.
        if let Err(error) = std::fs::write(path, json) {
            eprintln!("tpane profile: cannot write {}: {error}", path.display());
        }
    }
}
