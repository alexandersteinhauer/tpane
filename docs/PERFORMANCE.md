# Event-loop performance investigation

These are historical measurements with the original frame-rate cap. See [FRAME_INTERVAL.md](FRAME_INTERVAL.md) for its removal from both current implementations and fresh capped/uncapped comparisons.

## Scope and method

The tab implementation is preserved in commit `d992ddf`. The pre-tabs reference is `01b06cc`. All measurements use optimized release binaries on the same Apple M5 Pro / macOS 26.6.2 machine, with Rust 1.98.1 and the same pinned Ghostty engine.

`scripts/benchmark.py` runs the actual binary in a 100-column, 30-row PTY with an isolated, deterministic raw Python backend. It measures:

- Input write to delivery of the complete host frame containing its response: 80 samples with seeded 0–20 ms think times.
- Catch-up after a 100-key burst submitted in one write.
- A 2 MiB ASCII output workload, including delivery of its final frame.
- Idle CPU over two seconds, and CPU consumed by each workload.

Each primary case has three trials, with 1, 8, and 24 panes. Only the selected pane performs the workload; the others are idle. Runs are serial, never alongside compiler/test workloads. The pre-tabs reference supports only one pane and has 30 content rows rather than 29.

CPU comes from macOS `proc_pidinfo(PROC_PIDTASKINFO)`, includes all tpane threads, and excludes the Python backend and driver. The harness converts Mach ticks using `mach_timebase_info` and checks its own counter against Python's independent process CPU clock. 100% means one full core, not the whole machine.

These are PTY-to-frame-delivery measurements, **not physical Ghostty display latency or an end-to-end Neovim benchmark**. Driver scheduling, backend production, power/thermal state, and unrelated machine load remain possible influences. No driver overhead is subtracted. JSON files retain all samples, CPU counters, binary/harness hashes, and environment details.

## Verified regression

Median single-pane finite-output throughput fell from **0.86 MiB/s before tabs to 0.20 MiB/s after tabs**, roughly 4.3× slower.

The pre-tabs loop consumed up to four PTY chunks per pass. The tabbed loop consumed only one chunk per pane before waiting on host input for up to 4 ms, even when more child output was queued. This imposed a scheduling bottleneck independent of terminal parsing speed. It also repeatedly scanned idle children and performed render checks on a timer.

## Changes

- A dedicated blocking input reader feeds a bounded channel. Input and PTY workers wake the main thread when work arrives.
- The main loop drains output in round-robin passes, bounded by 64 chunks or approximately 2 ms per turn, then checks input again. It does not deliberately sleep while output remains queued.
- The idle loop parks until notified or until a render/maintenance deadline. Process-status checks are throttled to 100 ms; EOF still prompts checks. Shutdown signals are checked at most 100 ms later while idle.
- Hidden output does not schedule repainting the active terminal.
- Frame timing advances only when drawing actually emits a frame.

The final point was caught by measurement, not assumed: the first event-driven prototypes improved output throughput but worsened median input latency to about 17 ms. An empty render check was advancing the frame timer just before the child response arrived. Initial results are retained in `sync-wake-initial.json` and `tokio-wake-initial.json` rather than discarded.

## Tokio experiment

`scripts/benchmark-tokio.sh` generates an isolated copy of the current sources, pins Tokio 1.53.1, runs the loop as a future on a current-thread runtime, and replaces thread parking with `Notify` plus an async timer. Queues, processing budgets, rendering, and blocking PTY workers stay identical. The normal executable and production dependencies are not replaced by this experiment.

This tests **async event-loop orchestration**, not a complete `AsyncFd` PTY driver. A subsequent [AsyncFd PTY experiment](ASYNC_PTY.md) now compares actual asynchronous PTY tasks, memory/thread counts, and backpressure/shutdown behavior at up to 64 panes. Its results are separate from the orchestration-only results below.

## Results and decision

Final comparison is generated from the raw files in `docs/performance/` using `scripts/summarize-benchmark.py`. Reported values are medians across trials; p50/p95 columns are medians of each trial's percentile, not pooled percentiles. Small finite-output timings can be sensitive to the 16 ms frame boundary, so a separate 32 MiB workload compares sustained output for the two optimized candidates.

### Primary comparison

| Loop | Panes | Idle CPU, % of one core | Key p50, ms | Key p95, ms | 100-key burst, ms | 2 MiB output, MiB/s |
|---|---:|---:|---:|---:|---:|---:|
| Committed tabs baseline | 1 | 0.248 | 14.53 | 19.69 | 94.99 | 0.20 |
| Synchronous wakeups | 1 | 0.013 | 0.60 | 16.82 | 18.92 | 59.85 |
| Tokio wakeups | 1 | 0.027 | 2.00 | 17.62 | 19.80 | 114.75 |
| Committed tabs baseline | 8 | 0.518 | 14.57 | 19.93 | 19.90 | 0.20 |
| Synchronous wakeups | 8 | 0.025 | 1.00 | 17.86 | 18.37 | 120.95 |
| Tokio wakeups | 8 | 0.034 | 1.78 | 17.52 | 20.91 | 107.13 |
| Committed tabs baseline | 24 | 0.783 | 14.43 | 19.73 | 16.58 | 0.20 |
| Synchronous wakeups | 24 | 0.046 | 1.11 | 16.65 | 17.09 | 60.57 |
| Tokio wakeups | 24 | 0.055 | 1.95 | 17.74 | 18.69 | 117.48 |

The small output workload completes in roughly one or two frame intervals on the optimized builds. For example, the three single-pane synchronous throughput samples are **123.64, 59.85, and 55.50 MiB/s**. Do not interpret the corresponding Tokio median as proof of a 2× async processing advantage: a final frame arriving on the next 16 ms boundary can halve this finite-workload rate. Burst results are likewise not uniformly better at every pane count.

### Sustained 32 MiB output

| Loop | Panes | Median MiB/s | Trial range, MiB/s | CPU ms/MiB |
|---|---:|---:|---:|---:|
| Synchronous wakeups | 1 | 131.98 | 100.90–163.59 | 8.02 |
| Tokio wakeups | 1 | 139.45 | 131.64–141.45 | 8.45 |
| Synchronous wakeups | 24 | 141.26 | 132.02–151.70 | 9.17 |
| Tokio wakeups | 24 | 131.26 | 122.03–140.38 | 9.94 |

**Decision: keep the synchronous, event-driven loop as the default.** Tokio has no consistent measured advantage here: it is about 6% ahead on median sustained throughput with one pane and about 7% behind with 24 panes, with overlapping trial ranges. The synchronous variant has lower measured idle CPU and lower median key latency in the primary cases. Three trials do not support a statistical claim that one runtime is universally faster.

The large improvement comes from removing deliberate waits while work is queued, batching input/output, avoiding idle repaint checks, and not charging empty renders against the frame timer—not from adding an async runtime. The p95 input tail remains around 17 ms because rendering is still limited to approximately 60 FPS.

Both optimized variants have **4 threads with one pane and 50 with 24 panes**. This experiment does not test the potential thread/memory savings of a fully asynchronous PTY driver.

## Reproduce

Run on an otherwise idle machine. Do not run variants concurrently.

```sh
./scripts/cargo.sh build --release --locked
python3 scripts/benchmark.py --binary target/release/tpane \
  --panes 1,8,24 --trials 3 --keys 80 --idle-seconds 2 --output sync.json

./scripts/benchmark-tokio.sh
python3 scripts/benchmark.py --binary .tools/perf/tpane-tokio-wake \
  --panes 1,8,24 --trials 3 --keys 80 --idle-seconds 2 --output tokio.json

python3 scripts/summarize-benchmark.py sync.json tokio.json
```

For the sustained comparison, use `--panes 1,24 --trials 3 --keys 20 --idle-seconds .5 --output-bytes 33554432` with each optimized binary. Build historical references from their commits in separate worktrees; pass `--panes 1 --root-bar-rows 0` for `01b06cc`.

## Correctness boundary

Both the synchronous and Tokio integration suites passed all 18 tests after the event-loop changes, including hidden output, in-band resize, mouse input, and process cleanup. A regression test verifies that an empty draw is not counted as a presented frame. The existing intermittent `pty::tests::resize_reaches_slave` unit-test failure remains unresolved and was also observed during this investigation; performance results are not evidence that it is fixed. See `VALIDATION.md`.
