# Separating terminal-state work from asynchronous I/O

## Decision and implementation

The direct-readiness proposal was implemented and tested, but **did not recover uncapped visible-output throughput**. It is retained as an isolated diagnostic variant, not promoted as the solution.

The current Tokio candidate instead keeps terminal state/rendering on the caller of `Runtime::block_on` and runs Send-only I/O tasks on **one independent Tokio worker**. Ghostty objects do not move between threads. The normal synchronous implementation and shared renderer are unchanged.

- `experiments/tokio-native/runtime.rs` constructs a multi-thread runtime with one worker named `tpane-io`.
- `main.rs` calls `block_on(run())`. This future owns all Ghostty/pane/UI state and is not spawned onto the worker.
- Existing bounded PTY byte queues let I/O continue while the UI performs synchronous parsing/rendering. Per-pane credits and the shared event-channel limit remain intact.
- `host_input.rs` owns `EventStream` in a separate Tokio task and forwards through a bounded channel. Unrelated PTY wakeups no longer repoll Crossterm's reader mutex from the UI task.
- Host output remains asynchronous, ordered and limited to one incremental frame in flight. There is **no frame interval**, delayed flush timer, discarded incremental frame, or change to renderer semantics.
- Pane/status routing stays event-driven; no all-pane idle scans were introduced.

The execution boundary matters more than changing which queue is drained. In the current-thread design, rendering stops both the UI and its I/O tasks. Direct reads on that same thread do not solve that problem. An independent I/O worker can continue filling bounded queues during rendering, so the next UI turn has more useful work available without waiting to accumulate a batch.

Tokio documents that `Runtime::block_on` runs its future on the calling thread and does not require `Send`. Thus !Send Ghostty state does not require putting all I/O on a current-thread runtime. The new regression test deliberately occupies the UI caller while a spawned worker drives an actual AsyncFd readiness operation, and then verifies the UI remains on its original thread.

## Diagnostic comparison

Three uninstrumented trials, one pane, 32 MiB of changing ASCII log output. These initial comparisons establish the execution-model effect; the broader final matrix is recorded separately.

| Implementation | MiB/s | App CPU ms/MiB | Key p95 ms | Threads |
|---|---:|---:|---:|---:|
| Previous current-thread byte events | 38.16 | 22.32 | 0.18 | 2 |
| Current-thread direct readiness | 28.70 | 28.09 | 0.19 | 2 |
| Direct readiness with independent I/O worker | 34.42 | 31.20 | 0.18 | 3 |
| Byte events with independent I/O worker | 111.36 | 13.49 | 0.18 | 3 |

The last comparison changed only the runtime execution model, not the renderer, frame interval, input adapter or byte-event protocol. The final candidate additionally isolates EventStream polling and adds a runtime-affinity/progress regression test.

A mislabeled preliminary artifact was detected by SHA-256 comparison: Cargo reused an executable from a shared target directory when both source snapshots had been created before the first build. That observation is retained under `docs/performance/readiness/invalid/`, excluded from conclusions, and rerun after forcing source freshness. Corrected byte/readiness executables have distinct hashes. Future isolated variant builds must ensure source freshness or use separate targets.

## Rejected readiness variant

The readiness-only transport issues one non-cloneable lease per pane. The coordinator retains leases across bounded round-robin turns, reads directly until WouldBlock or a 64-read / approximately 2 ms fairness budget, and rearms on lease drop. It uses a reusable 16 KiB buffer and does not allocate/copy per-read messages.

It passed the existing integration tests and new tests covering:

- Single outstanding notification, cancellation, channel closure and descriptor release.
- Exact bytes before EOF, genuine read errors and real PTY hangup.
- Rearming whether data arrives before or after acknowledgement.
- Token retention at a bounded-turn boundary, round-robin service, stale notifications on pane removal, and hidden output remaining render-clean.

It still performed poorly in the measured visible-output workload. Passing those tests is not evidence of a performance fix. The variant is retained through `scripts/benchmark-tokio-native.sh --readiness` and `experiments/tokio-native/readiness.patch`.

## Final validation matrix

The final candidate includes the independent I/O worker, isolated input task, named worker and runtime ownership/progress regression test. All binaries remain uncapped. Three trials per case ran serially, with the corrected reader and no concurrent profiling/builds. The 32 MiB visible workload varies request numbers rather than relying on identical log lines. Latency percentiles are computed per trial and then summarized by their median.

### Visible changing output

| Implementation | Panes | MiB/s | App CPU ms/MiB |
|---|---:|---:|---:|
| Previous current-thread Tokio | 1 | 39.87 | 21.94 |
| Independent-I/O Tokio | 1 | 112.60 | 13.23 |
| Synchronous control | 1 | 145.59 | 9.74 |
| Previous current-thread Tokio | 24 | 39.31 | 21.77 |
| Independent-I/O Tokio | 24 | 118.70 | 12.67 |
| Synchronous control | 24 | 153.52 | 9.60 |
| Previous current-thread Tokio | 64 | 37.85 | 22.44 |
| Independent-I/O Tokio | 64 | 108.44 | 13.34 |
| Synchronous control | 64 | 149.27 | 10.23 |

The single-pane candidate improves throughput **2.82×** and reduces app CPU cost per MiB about **40%** relative to the uncapped current-thread implementation. It remains approximately **23% below synchronous throughput** in that case; the renderer itself was not optimized.

### Quiet input and idle state

| Implementation | Panes | Key p50 ms | Key p95 ms | Threads | Idle CPU % |
|---|---:|---:|---:|---:|---:|
| Previous current-thread Tokio | 1 | 0.11 | 0.18 | 2 | <0.001 |
| Independent-I/O Tokio | 1 | 0.10 | 0.19 | 3 | <0.001 |
| Synchronous | 1 | 0.09 | 0.16 | 4 | 0.013 |
| Previous current-thread Tokio | 24 | 0.11 | 0.20 | 2 | <0.001 |
| Independent-I/O Tokio | 24 | 0.10 | 0.17 | 3 | <0.001 |
| Synchronous | 24 | 0.08 | 0.14 | 50 | 0.046 |
| Previous current-thread Tokio | 64 | 0.11 | 0.17 | 2 | <0.001 |
| Independent-I/O Tokio | 64 | 0.10 | 0.17 | 3 | <0.001 |
| Synchronous | 64 | 0.10 | 0.18 | 130 | 0.110 |

Eight-pane results are retained in the raw data. Idle CPU is below reporting precision in these samples, not a guarantee of literally zero CPU. At 64 panes, idle RSS is 28.08 MiB for the candidate versus 27.94 MiB previously and 29.89 MiB synchronously.

### Hidden busy producers

| Implementation | Panes / busy | Accepted MiB/s | App CPU % of one core | Key p95 ms |
|---|---:|---:|---:|---:|
| Previous current-thread Tokio | 8 / 4 | 130.05 | 77.79 | 0.32 |
| Independent-I/O Tokio | 8 / 4 | 176.15 | 133.85 | 0.25 |
| Synchronous | 8 / 4 | 174.03 | 253.81 | 0.22 |
| Previous current-thread Tokio | 24 / 8 | 158.30 | 87.82 | 0.41 |
| Independent-I/O Tokio | 24 / 8 | 158.01 | 126.09 | 0.42 |
| Synchronous | 24 / 8 | 94.41 | 270.06 | 0.31 |

The independent worker is **not free**: at 24/8 it uses about 44% more CPU than the prior current-thread version for essentially equal accepted throughput. It still uses much less CPU and delivers more aggregate output than the synchronous control. Accepted-byte throughput includes bounded buffering; every producer's ordered DONE verification passed. These paced-input cases do not measure keyboard latency during visible flooding; existing Ctrl-C/next-command integration tests cover responsiveness there without providing a percentile claim.

### Correctness and scope

- Final candidate: **56 unit tests + 21 integration tests passed**, serially; Clippy passed with warnings denied.
- Tests include I/O progress while the !Send UI caller is occupied, frame ordering/backpressure, exact paste delivery, child lifecycle, terminal restoration, blocked-host input/shutdown, and Neovim resize.
- Rejected direct-readiness variant: **68 unit tests + 21 integration tests passed**, including the added bounded-turn and stale-token tests. It was not selected on performance grounds.
- The normal synchronous binary and shared renderer are unchanged by this execution-model fix. The default isolated Tokio candidate is `.tools/perf/tpane-tokio-native`.
- Three trials on one machine do not establish universal superiority. Physical Ghostty display latency and a manual daily-driver editor session remain outside these measurements. Previously documented parallel PTY stress findings are not claimed fixed.

Raw final observations are in `docs/performance/readiness/final/`; all 81 final benchmark trials completed. No sampled or instrumented timings are included in these tables.

## Reproduce

```sh
# Current candidate: caller-owned UI and one independent I/O worker
./scripts/benchmark-tokio-native.sh

# Historical current-thread byte-event comparison
./scripts/benchmark-tokio-native.sh --byte-events

# Rejected direct-readiness design
./scripts/benchmark-tokio-native.sh --readiness
```

Regenerate the default candidate before running its test suite or profiling. The variants reuse a private generated project, not the production project. The retained profiler/diagnostic tools for older byte-event implementations may require their historical source snapshot.

Raw comparisons, tests and final measurements live in `docs/performance/readiness/`. Profiles/sampling, where used, remain separate from normal benchmark medians. Throughput measures child payload through completed host PTY delivery, not physical display. CPU includes all tpane threads but excludes the backend/driver; 100% represents one core.
