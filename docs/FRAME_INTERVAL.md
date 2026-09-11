# Removing the frame interval

Both current implementations now render without an artificial frame-rate cap:

- Synchronous: render dirty visible state after each bounded input/output work batch. Idle waiting remains; the 100 ms timeout is for child-status checks, not rendering.
- Tokio-native: render dirty visible state as soon as the host writer is free. Retain one incremental frame in flight, ordered writes, bounded queues, and cooperative event budgets. No frame timer or frame-completion timestamp remains.

This does not force one frame per byte or drop incremental frames. Batching and natural host backpressure still coalesce updates. Neither implementation paints unchanged state in an idle loop.

## Validation

Release tests ran serially, with no debugger interventions:

- Synchronous: 39 unit tests and 20 integration tests passed.
- Tokio-native: 53 unit tests and 21 integration tests passed, including blocked-host input/shutdown, frame ordering, partial writes, cancellation and descriptor-flag restoration.
- Both passed Clippy with warnings denied.

These runs do not establish that the previously documented parallel PTY stress issues are fixed.

## Comparison method

Four release binaries: synchronous capped/uncapped and Tokio-native capped/uncapped. Capped binaries were frozen before editing; they are the exact previously measured builds. All four are rebenchmarked here rather than comparing only against older observations. The initial run used the existing harness unchanged. A verified reader bottleneck then required a harness correction (below); quiet and finite-output cases are repeated with that correction for all four unchanged binaries. Workloads remain identical.

- Quiet panes: 1 / 8 / 24 / 64 panes, three trials each, 80 keys with seeded 0–20 ms think time, two-second idle sample, and 2 MiB finite output.
- Sustained visible output: 1 / 24 / 64 panes, three trials each, one active 32 MiB producer.
- Hidden busy producers: 8 panes / 4 producers and 24 panes / 8 producers, three five-second trials each, 80 paced foreground keys, followed by ordered DONE verification for every producer.
- Runs execute serially without profiling or concurrent builds. Tables use medians across trials; latency percentiles are calculated per trial before taking their median.
- Latency ends at completed host PTY-frame delivery, not physical display. CPU covers all tpane threads, excludes the driver/backends, and uses 100% for one core. Busy accepted-byte throughput includes bounded buffering; post-stop drain verification is separate.
- Host bytes read are retained in each phase so the extra terminal-output cost of rendering more frequently can be checked, not just latency and producer throughput.

Raw observations and a patch that restores the cap are in `docs/performance/frame-interval/`. The patch is for a disposable copy of the current source, not an instruction to modify the working tree during a benchmark. Frozen binaries are under `.tools/perf/frame-interval/`.

## Benchmark-reader correction

The initial uncapped synchronous 32 MiB result was only 30.81 MiB/s, with tpane consuming about 28% of one core. It emitted approximately 5.14 MiB to the host versus 0.04 MiB when capped. Before treating that as an application processing limit, the reader was profiled separately and LLDB attached during a longer output run.

- LLDB stopped the synchronous main thread in `write`, called by `App::draw`. Its PTY reader was blocked on the bounded channel. This located the downstream backpressure, not a parser busy loop.
- The Python driver profile spent **7.755 of 9.481 seconds** in regex `search`, with another **1.411 seconds** directly in `HostStream.read`. The old `wait` repeatedly rescanned up to 1 MiB of already-checked frames on every read.
- `HostStream.wait` now discards already-searched complete frames only when no marker matched. It retains the unfinished frame, split markers/terminators, a matched marker awaiting completion, and all frames following a successful match.
- Five reader regression tests cover arbitrary split points, byte-at-a-time completion, bounded retention over more than 1 MiB of irrelevant frames, subsequent buffered frames, and incomplete matches. They pass.
- New phase records separately include driver CPU. It is not added to tpane CPU or subtracted from elapsed time.

Raw initial observations remain at the top level of `docs/performance/frame-interval/`; do not use their visible-output throughput as an isolated tpane speed comparison. Corrected quiet/finite-output comparisons are in `corrected-reader/`. The original busy-pane results have negligible host traffic and are reported separately. LLDB/cProfile runs are diagnostic only and are not included in benchmark medians. Transcripts and profiles are in `diagnostic/`.

## Corrected results

### Quiet input latency

Milliseconds, median of three trials' p95 values. The median key response after removal is 0.07–0.09 ms across these cases.

| Implementation | Panes | Capped p95 | Uncapped p95 |
|---|---:|---:|---:|
| Synchronous | 1 | 17.61 | 0.12 |
| Synchronous | 8 | 17.22 | 0.12 |
| Synchronous | 24 | 17.14 | 0.14 |
| Synchronous | 64 | 17.51 | 0.15 |
| Tokio-native | 1 | 16.29 | 0.15 |
| Tokio-native | 8 | 16.80 | 0.12 |
| Tokio-native | 24 | 16.04 | 0.17 |
| Tokio-native | 64 | 17.13 | 0.14 |

Idle CPU and thread counts remain essentially unchanged. In particular, removing the frame timer did not introduce an idle render spin.

### Sustained visible output

One active producer writes 32 MiB. Unlike hidden-producer throughput, these measurements include continuous repainting of its visible terminal.

| Implementation | Panes | Capped MiB/s | Uncapped MiB/s | Capped CPU ms/MiB | Uncapped CPU ms/MiB |
|---|---:|---:|---:|---:|---:|
| Synchronous | 1 | 140.60 | 152.60 | 7.68 | 9.21 |
| Synchronous | 24 | 131.61 | 144.10 | 9.53 | 9.99 |
| Synchronous | 64 | 163.12 | 142.82 | 8.72 | 10.27 |
| Tokio-native | 1 | 125.22 | 39.91 | 5.85 | 21.94 |
| Tokio-native | 24 | 134.07 | 41.32 | 5.42 | 21.35 |
| Tokio-native | 64 | 125.17 | 40.44 | 5.95 | 21.71 |

The synchronous single-pane throughput samples were 152.79 / 105.13 / 140.60 MiB/s capped and 157.58 / 134.03 / 152.60 MiB/s uncapped. Their ranges overlap: the median increase is not evidence of a universal speedup. The earlier apparent fall to 30.81 MiB/s was largely a reader bottleneck, not a new synchronous parser limit.

Tokio-native has a **real visible-output regression in the corrected harness**: approximately 68% less single-pane throughput, with about 3.75 times the app CPU cost per MiB. Its single-pane uncapped samples were consistently 40.25 / 39.35 / 39.91 MiB/s. No frame cap has been silently restored to hide this result.

### Host-output cost

Single-pane 32 MiB workload, medians. App CPU includes all its threads; driver CPU is shown separately.

| Implementation | Mode | Host output MiB | App CPU seconds | Driver CPU seconds |
|---|---|---:|---:|---:|
| Synchronous | Capped | 0.044 | 0.246 | 0.00068 |
| Synchronous | Uncapped | 5.598 | 0.295 | 0.05194 |
| Tokio-native | Capped | 0.047 | 0.187 | 0.00066 |
| Tokio-native | Uncapped | 20.327 | 0.702 | 0.17784 |

The faster reader changes the natural backpressure too: native uncapped emits far more intermediate state than with the slow reader. Its current eager render scheduling no longer gets as much coalescing from a blocked host writer. These observations show that host consumption speed matters; they are not physical Ghostty display benchmarks. More host output and app CPU explain why removing a timer need not improve bulk throughput. The next optimization candidate is work/frame coalescing without an artificial deadline, not claiming that the regression disappeared.

In the corrected diagnostic run, driver regex search consumed **0.032 seconds**, versus **7.755 seconds** before the reader correction. Most elapsed driver time was now waiting in `select`. Diagnostic run durations are not benchmark speed comparisons.

### Hidden busy panes

These are the original five-second busy runs. Every trial delivered exactly 12,960 host bytes during the steady phase, so the large-output reader bottleneck did not arise here. Keys are paced farther apart than the old frame interval.

| Implementation | Panes / busy | Capped → uncapped accepted MiB/s | Capped → uncapped app CPU % | Capped → uncapped key p95 ms |
|---|---:|---:|---:|---:|
| Synchronous | 8 / 4 | 176.78 → 180.03 | 252.06 → 256.38 | 0.21 → 0.20 |
| Synchronous | 24 / 8 | 92.90 → 92.57 | 273.49 → 272.71 | 0.29 → 0.29 |
| Tokio-native | 8 / 4 | 127.31 → 128.61 | 76.50 → 77.10 | 0.32 → 0.29 |
| Tokio-native | 24 / 8 | 156.75 → 151.78 | 87.58 → 86.35 | 0.40 → 0.44 |

These differences are modest compared with the visible-output regression. All producer DONE checks passed. At 24/8, final verification time fell from approximately 152 to 3.2 ms synchronously and 144 to 3.1 ms natively. This includes sequential tab selection and rendering, not just processing an output backlog.

## Conclusion

The follow-up [native render diagnosis](NATIVE_RENDER_DIAGNOSIS.md) profiles the visible workload, identifies excessive frame construction and input-lock contention, and records both positive and negative scheduling experiments.

The requested cap removal is retained in both implementations. It removes the approximately one-frame latency tail, with no observed idle-work regression. Synchronous visible-output throughput is broadly maintained at higher CPU/host-output cost; native eager rendering substantially reduces visible-output throughput. Both remain correct in the automated test suite, but the uncapped native renderer needs better coalescing before claiming a general performance advantage.

## Reproduce

Build current uncapped binaries with `./scripts/cargo.sh build --release --locked` and `./scripts/benchmark-tokio-native.sh`. The former is `target/release/tpane`; the latter is `.tools/perf/tpane-tokio-native`.

For each capped/uncapped binary, run:

```sh
python3 scripts/benchmark.py --binary BINARY --panes 1,8,24,64 \
  --trials 3 --keys 80 --idle-seconds 2 --output quiet.json
python3 scripts/benchmark.py --binary BINARY --panes 1,24,64 \
  --trials 3 --keys 20 --idle-seconds .5 --output-bytes 33554432 \
  --output sustained.json
python3 scripts/benchmark-busy.py --binary BINARY --panes 8 --busy 4 \
  --trials 3 --seconds 5 --keys 80 --output busy8.json
python3 scripts/benchmark-busy.py --binary BINARY --panes 24 --busy 8 \
  --trials 3 --seconds 5 --keys 80 --output busy24.json
```

Use separate output paths for every variant. `scripts/summarize-async-pty.py` summarizes the quiet/sustained JSON; `scripts/summarize-busy.py` summarizes busy cases.
