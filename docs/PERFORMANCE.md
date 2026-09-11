# Performance

## Decision and measurement boundary

The sole application is now independent-I/O Tokio: caller-owned Ghostty/UI state plus one I/O worker. Promotion removes maintained variants; it does **not** establish that Tokio is faster in every workload or that the remaining overhead has been fixed.

The tables below summarize historical release measurements from the final execution-model comparison, preserved at `cbeb68a` and checkpoint `691c180`. They are not new benchmark or test results for the promoted checkout. Measurements used an Apple M5 Pro / macOS 26.6.2, Rust 1.98.1, and the same pinned Ghostty engine. All three implementations were uncapped; three trials per case ran serially with the corrected benchmark reader, without concurrent builds/profiling. Values are medians; latency percentiles are computed per trial and then summarized by their median, not pooled.

The harness runs the binary in a PTY with deterministic Python children. Latency ends at delivery of the **complete host frame**, not physical Ghostty display. CPU includes all tpane threads but excludes driver/children; 100% is one core. Libproc task CPU units are checked against an independent process clock. There is no driver-overhead subtraction. Three trials on one machine do not establish universal superiority, statistical significance, or end-to-end Neovim latency.

## Measured tradeoffs

### Visible changing output (32 MiB)

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

At one pane, independent I/O improved throughput **2.82×** and reduced CPU per MiB about **40%** versus current-thread Tokio, but remained about **23% slower** than synchronous throughput. Rendering previously stopped both the UI and its current-thread I/O tasks. The worker lets bounded byte queues fill during synchronous UI work; the renderer itself was not optimized.

### Quiet input and idle state

| Implementation | Panes | Key p50 / p95 ms | Threads | Idle CPU % |
|---|---:|---:|---:|---:|
| Current-thread Tokio | 1 | 0.11 / 0.18 | 2 | <0.001 |
| Independent-I/O Tokio | 1 | 0.10 / 0.19 | 3 | <0.001 |
| Synchronous control | 1 | 0.09 / 0.16 | 4 | 0.013 |
| Current-thread Tokio | 64 | 0.11 / 0.17 | 2 | <0.001 |
| Independent-I/O Tokio | 64 | 0.10 / 0.17 | 3 | <0.001 |
| Synchronous control | 64 | 0.10 / 0.18 | 130 | 0.110 |

The async implementation avoids per-pane I/O threads. Below reporting precision is not literally zero CPU. At 64 panes, measured idle RSS was 28.08 MiB for independent I/O, 27.94 MiB for current-thread Tokio, and 29.89 MiB synchronously; snapshots are not peak memory. Intermediate pane counts are in the archive.

### Hidden busy producers, quiet foreground

| Implementation | Panes / busy | Accepted MiB/s | App CPU % | Key p95 ms |
|---|---:|---:|---:|---:|
| Current-thread Tokio | 8 / 4 | 130.05 | 77.79 | 0.32 |
| Independent-I/O Tokio | 8 / 4 | 176.15 | 133.85 | 0.25 |
| Synchronous control | 8 / 4 | 174.03 | 253.81 | 0.22 |
| Current-thread Tokio | 24 / 8 | 158.30 | 87.82 | 0.41 |
| Independent-I/O Tokio | 24 / 8 | 158.01 | 126.09 | 0.42 |
| Synchronous control | 24 / 8 | 94.41 | 270.06 | 0.31 |

At 24/8, independent I/O used about **44% more CPU** than current-thread Tokio for essentially equal accepted throughput. It still used substantially less CPU and delivered more aggregate output than the synchronous control. Accepted-byte throughput includes bounded buffering; each producer's ordered final DONE verification passed in those historical runs. These are not input-latency percentiles during visible flooding.

## Follow-up: remaining overhead is not fixed

The hidden-output investigation at `691c180` located substantial system-call and cross-thread wake/park work, with tiny reader/receiver transfers. Separate instrumentation found only 130 render calls versus 836,346 terminal-feed calls in the control; rendering was not the hidden-workload bottleneck. Stack samples and inclusive wall spans are diagnostic evidence, not exact CPU attribution or ordinary benchmark timings.

Reducing successful-read yields to once every 64 reads was **rejected**. In a separate three-trial 24/8 comparison, throughput rose from 165.54 to 170.67 MiB/s but CPU rose from 130.40% to 140.47%; CPU per MiB worsened from 7.88 to 8.23 ms. Visible throughput fell from 120.69 to 109.34 MiB/s. Instrumented runs found 3.57× more WouldBlock results per MiB and smaller receive batches. These separate runs must not be mixed with the earlier matrix.

Direct-readiness transport also failed to recover visible throughput. Neither it nor the yield change is part of the promoted implementation. Publication/allocation granularity and cross-thread handoffs remain possible investigation targets, not demonstrated fixes. No frame cap, sleep-based batching, extra I/O workers, or renderer optimization is claimed to solve this.

## Run current benchmarks

Build first, then run on an otherwise idle machine, serially. Do not run tests, compilers, profilers, or other variants concurrently. Use fresh output paths.

```sh
./scripts/cargo.sh build --release --locked
python3 scripts/test-benchmark.py
python3 scripts/benchmark.py --binary target/release/tpane \
  --panes 1,8,24 --trials 3 --keys 80 --idle-seconds 2 --output quiet.json
python3 scripts/benchmark.py --binary target/release/tpane \
  --panes 1,24,64 --trials 3 --keys 20 --idle-seconds .5 \
  --output-bytes 33554432 --output visible.json
python3 scripts/benchmark-busy.py --binary target/release/tpane \
  --panes 8 --busy 4 --trials 3 --seconds 5 --output busy8.json
python3 scripts/benchmark-busy.py --binary target/release/tpane \
  --panes 24 --busy 8 --trials 3 --seconds 5 --output busy24.json
python3 scripts/summarize-benchmark.py quiet.json visible.json
python3 scripts/summarize-busy.py busy8.json busy24.json
```

The current visible-output fixture varies request IDs across 1,024 log lines per block, incorporating the formerly separate changing-output experiment. It no longer emits a block of identical lines. Do not compare its throughput directly with historical repeated-line measurements. Exact historical reproduction requires the archived source/harness and workload details. Binary and harness hashes, raw samples, environment, and CPU/memory counters belong with each new result. Automated correctness and manual host validation remain separate: see [VALIDATION.md](VALIDATION.md).

## Historical evidence

Detailed investigations, raw JSON, profiles, invalid observations, patches, variant builders, and diagnostic scripts are preserved in git at **`691c180`**; **`cbeb68a`** is the earlier checkpoint before the hidden-I/O investigation. They are deliberately removed from the working tree to keep the checkout compact.

```sh
# Read an archived investigation without restoring obsolete files.
git show 691c180:docs/IO_EXECUTION.md
git show 691c180:docs/HIDDEN_IO.md

# List or extract the detailed evidence outside this checkout.
git ls-tree -r --name-only 691c180 docs/performance
mkdir -p /tmp/tpane-performance-history
git archive 691c180 docs/performance | tar -x -C /tmp/tpane-performance-history

# Recover the matching sources, harnesses and variant builders when needed.
git worktree add --detach ../tpane-history 691c180
```

Key archived paths: `docs/performance/readiness/final/` for the tables, `docs/performance/hidden-io/` for the rejected yield experiment, and the earlier `frame-interval/`, `native-render-diagnosis/`, `tokio-native/`, and `async-pty/` investigations. Old capped or flawed-reader results are historical only. Invalid stale-binary and thread-CPU-unit artifacts remain labeled in the archive and are excluded from conclusions; use separate build targets when reproducing variants to avoid stale artifact reuse.
