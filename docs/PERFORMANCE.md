# Performance

## Decision and measurement boundary

The sole application is now independent-I/O Tokio: caller-owned Ghostty/UI state plus one I/O worker. Promotion removes maintained variants; it does **not** establish that Tokio is faster in every workload or that the remaining overhead has been fixed.

The tables below summarize historical release measurements from the final execution-model comparison, preserved at `8c143c6` and checkpoint `3c9b585`. They are not new benchmark or test results for the promoted checkout. Measurements used an Apple M5 Pro / macOS 26.6.2, Rust 1.98.1, and the same pinned Ghostty engine. All three implementations were uncapped; three trials per case ran serially with the corrected benchmark reader, without concurrent builds/profiling. Values are medians; latency percentiles are computed per trial and then summarized by their median, not pooled.

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

The hidden-output investigation at `3c9b585` located substantial system-call and cross-thread wake/park work, with tiny reader/receiver transfers. Separate instrumentation found only 130 render calls versus 836,346 terminal-feed calls in the control; rendering was not the hidden-workload bottleneck. Stack samples and inclusive wall spans are diagnostic evidence, not exact CPU attribution or ordinary benchmark timings.

Reducing successful-read yields to once every 64 reads was **rejected**. In a separate three-trial 24/8 comparison, throughput rose from 165.54 to 170.67 MiB/s but CPU rose from 130.40% to 140.47%; CPU per MiB worsened from 7.88 to 8.23 ms. Visible throughput fell from 120.69 to 109.34 MiB/s. Instrumented runs found 3.57× more WouldBlock results per MiB and smaller receive batches. These separate runs must not be mixed with the earlier matrix.

Direct-readiness transport also failed to recover visible throughput. Neither it nor the yield change is part of the promoted implementation. No frame cap, sleep-based batching, extra I/O workers, or renderer optimization is claimed to solve this.

## Transport event coalescing (current checkout)

`transport.rs` now publishes one bus event per readiness drain instead of one per read. Each reader reads directly into an uninitialized 32 KiB event buffer until the buffer is full, readiness is exhausted, or a 64-read budget is hit; the per-read `to_vec` allocation is gone, and the reader yields once per published event rather than per successful read. One credit still bounds one event, so queued bytes per pane remain bounded by 16 events.

Event size was measured explicitly because it sets the per-event fixed costs (readiness re-arm, bus send, credit cycle, wake) per byte moved. Against 16 KiB, eight-trial interleaved runs measured **32 KiB** at output time 1.3–3.2% faster at 1–8 tabs and flat at 24, with nvim-scroll CPU unchanged. **64 KiB** regressed 1–8-pane flood output 3–4.5% in two independent runs — once with per-event allocations, once again with fully recycled buffers — so the cost is the event length itself (each render and host write covers more coalesced output), not allocation, and it was rejected. An adaptive 16→64 KiB drain that grew only when full was **rejected** for the same reason.

Event buffers are now recycled: `PtyEvent::Output` carries a pooled buffer that returns to a small per-pane pool when the consumer drops it, so steady-state output allocates nothing. Eight-trial interleaved runs against per-event allocation measured recycling neutral at 1 tab, output 1.3% faster at 8 tabs, and 24-pane app CPU 4.603 → 4.411 s (−4.2%) with output time 2.825 → 2.660 s and throughput +6.2% — trial spread at 24 panes is wide, so treat that output delta as directional. nvim-scroll CPU moved 283.7 → 272.4 ms (−4.0%, overlapping trials).

A variant that also yielded between successful reads was measured and **rejected**: at 24 tabs the worker re-entered the reactor between reads, and output time rose from 3.1 s to 4.4 s (profiled) with no CPU win. The promoted form keeps the per-event yield only.

Adjacent three-trial `just bench --variants tpane` runs on the same machine (32 MiB/tab, original binary vs current build; medians; this machine was not otherwise idle, so low-pane deltas are within noise):

| Tabs | Output time | Throughput | App CPU | App CPU | Outer host CPU |
|---:|---:|---:|---:|---:|---:|
| 1 | 0.253 → 0.257 s | 126.3 → 124.5 MiB/s | 0.384 → 0.386 s | 12.00 → 12.07 ms/MiB | 0.071 → 0.070 s |
| 8 | 0.909 → 0.852 s | 281.6 → 300.6 MiB/s | 1.574 → 1.530 s | 6.15 → 5.98 ms/MiB | 0.175 → 0.162 s |
| 24 | 3.424 → 2.817 s | 224.3 → 272.6 MiB/s | 5.048 → 4.614 s | 6.57 → 6.01 ms/MiB | 0.370 → 0.272 s |

The 24-tab case improves output time about **18%** and app CPU about **8.6%**; eight tabs improve modestly; one tab is flat (little to coalesce, one extra `EAGAIN` probe per event). Per-thread libproc counters from sampled runs at 24 tabs: unix syscalls rose from 1381 to 2493 per MiB (the probes), while io-worker CPU fell from 3139 to 2697 ms; context switches stayed in the same range (158 → 190 per MiB). In the same-session `native,tpane` comparison, 24-tab tpane/native CPU was 2.04× and output time 1.13× (original comparison: 2.24× and 1.36×, different load window). Quiet key p95 moved in both directions within run-to-run machine noise (native-relative key p95 was 1.01–1.29×). Publication granularity is therefore a demonstrated, if modest, lever.

## Renderer fast paths (current checkout)

`render.rs` now reads one row-level styled/grapheme/selection state per dirty row instead of querying style, fg, bg, and selection for every cell. Unstyled rows share one frame-level base style; styled rows cache the full style by style id; selection applies one row-local range; grapheme-free rows encode the raw codepoint directly; wide spacer tails are inferred from the preceding wide cell; and the pen style carries across absolutely positioned rows, so a repeated style is emitted once per frame instead of once per row. A test-only copy of the previous renderer remains for differential tests, and `render::tests::microbench_render_paths` times full repaints.

Measured repaint cost (release, unperturbed 100x28 full frame): **25.6 us and 3027 bytes versus 44.3 us and 3135 bytes** for the previous renderer, a **1.73x** per-frame improvement; a busier re-run measured 1.6x.

Interactive workloads gain directly, because frames there are key-driven rather than flood-driven. `scripts/benchmark-programs.py` runs real programs over a raw PTY host (no outer emulator); its `nvim-scroll` scenario drives a scripted nvim pane (20,000-line file, 600 single-line scroll keys at 8 ms think time). Three interleaved trials per binary measured (medians):

| Metric | Previous | Current |
|---|---:|---:|
| tpane CPU (600 keys) | 265.2 ms | **218.8 ms** (−17.5%) |
| Main-thread CPU | 149.9 ms | **110.8 ms** (−26.1%) |
| io-thread CPU | 97.8 ms | 91.9 ms |
| Frames / host bytes | 1188 / 4.91 MB | 1190 / 4.91 MB |
| Key latency mean / p95 | 0.358 / 0.490 ms | 0.337 / 0.447 ms |

The same suite covers thirteen more program shapes. Three interleaved trials per binary, medians; the direction is consistent, but scenarios whose whole run costs only milliseconds of tpane CPU sit at the noise floor. Those three (`fzf-query`, `lazygit-nav`, `htop-nav`) were then scaled up (500,000-line fzf list, 80-commit lazygit repository driven through diff views, longer htop window) and re-measured with five interleaved trials; their medians are reported from that second run and remain unresolved rather than claimed as changes:

| Scenario | Main-thread CPU | Δ | Host bytes |
|---|---:|---:|---:|
| nvim-scroll | 149.9 → 110.8 ms | −26.1% | 0% |
| nvim-typing | 5.1 → 4.0 ms | −21.6% | −6% |
| less-page | 30.9 → 23.2 ms | −24.9% | −5% |
| tail-f | 286.6 → 196.3 ms | −31.5% | −4% |
| progress-bar | 80.2 → 63.4 ms | −20.9% | 0% |
| git-log-flood | 43.2 → 35.6 ms | −17.6% | +7% |
| style-churn | 91.6 → 80.7 ms | −11.9% | +10% |
| tmux-nested | 10.1 → 9.1 ms | −9.9% | −1% |
| wide-emoji | 24.0 → 24.4 ms | +1.7% | +20% |
| cat-flood | 220.3 → 217.0 ms | −1.5% | +32% |
| seq-flood | 1991.8 → 1989.7 ms | −0.1% | +31% |
| fzf-query | 6.8 → 7.0 ms | noise | +2% |
| lazygit-nav | 16.7 → 15.8 ms | noise | +2% |
| htop-nav | 10.5 → 9.0 ms | noise | −8% |

The synthetic flood benchmark and the suite's own flood scenarios trade the other way at some sizes. Interleaved eight-trial `just bench --variants tpane` runs (32 MiB/tab) measured 1 tab 0.259 → 0.248 s output and 0.397 → 0.378 s app CPU; 8 tabs 0.834 → 0.868 s and 1.511 → 1.541 s; 24 tabs flat (2.696 → 2.704 s). A diagnostic build of the benchmark host that counts host input shows why: faster draws let the render-priority loop repaint the same rows more often under sustained flood, so a 16 MiB two-case run emitted **5192 frames and 15.4 MB instead of 3630 frames and 11.2 MB**, and outer-host CPU rose 19–49%; the suite's `cat-flood` and `seq-flood` show the same +32% host bytes. Frame granularity, not per-cell query cost, sets flood cost. The fast paths are kept for the interactive win with that trade-off recorded rather than presented as a universal speedup.

## Frame buffer reuse (current checkout)

Each presented frame used to allocate twice: `App::draw` staged into a fresh `Vec`, and the main loop allocated another buffer that the host writer dropped after its write. `App::draw` now reuses one staging buffer, the main loop clears and refills a single buffer, and the writer hands the frame buffer back through the `FrameWritten` completion event (only one frame is ever in flight), so the steady-state render path no longer allocates per frame. The renderer's grapheme scratch `String` is reused the same way.

Interleaved eight-trial `just bench --variants tpane` runs (32 MiB/tab) measure app CPU 0.391 → 0.384 s at 1 tab, 1.543 → 1.517 s at 8 tabs, and 4.431 → 4.372 s at 24 tabs (−1.3 to −1.7%), with output time down about 1%. Under flood the win is small because large frames are dominated by the staging copy itself. Small key-driven frames are where the allocation used to bite: five-trial `nvim-scroll` runs (600 keys) measure main-thread CPU 161.4 → 141.5 ms (−12.3%) and total tpane CPU 289.4 → 267.9 ms (−7.4%) with frames, host bytes, and key latency unchanged. Post-change sampling attributes the remaining flood self time in `App::draw` to the row-emit copy, not allocation.

## Native / tpane / tmux / zellij

```sh
just bench
just bench --panes 1,8,24 --trials 5 --mib 64 --keys 100
# Correctness smoke only:
just bench --panes 1,3 --trials 1 --mib 1 --keys 3
python3 scripts/test-bench.py
python3 scripts/test-bench-producer.py
cc -std=c11 -O2 -Wall -Wextra -Werror scripts/test-bench-host.c -o /tmp/test-bench-host
/tmp/test-bench-host target/release/bench-host
```

Requires `just`, Python 3, tmux and Zellij on PATH, plus tpane's normal build
prerequisites. Zellij's isolated configuration targets the current CLI (tested
with 0.44.3). Missing tools fail explicitly; the default never silently omits a
competitor. `--variants native,tpane` is available for diagnostic subset runs.

`just bench` builds release tpane, obtains **its actual native static-library path
from Cargo's build messages**, and compiles `scripts/bench-host.c` with Clang `-O3`
against that same `libghostty-vt.a`. The C executable calls libghostty's C API
directly: no Rust wrapper, tpane engine, or tpane renderer in the native host.
It self-tests parsed-screen completion handling before running cases.

```text
native: N child PTYs -> N independent libghostty terminals in one C poll loop
others: N child PTYs -> multiplexer -> outer host PTY -> C libghostty host
```

The C host drains the outer PTY directly in bounded reads. Python only initiates
work and receives completion notifications, not the output byte stream. The same
changing-log fixture runs in all four cases. Every child's current 100×28 viewport
is verified at output completion; tpane now uses that same full-size outer PTY
without reserved bar rows. Historical measurements above predate this UI change. tmux
status and Zellij frames/bar/background plugins are disabled. Scrollback limits
are 10,000. Fresh HOME/config/runtime directories and private mux sessions prevent
user configuration or existing sessions from influencing the tests.

**Every tab produces output concurrently**, including hidden tabs. `--mib` is
per tab: the default workload is 32 MiB × the requested number of tabs. Each
fixture has a private control socket. All receive the same monotonic start
deadline before any output begins; no tab switching or serial per-tab triggers
are used during measurement. Shared timestamps use POSIX `CLOCK_MONOTONIC`;
Python 3.9's macOS `perf_counter()`/`monotonic()` epochs are process-local and
must not be used to coordinate different producers. Actual producer processing intervals must overlap,
otherwise the run fails with a request to increase the workload.

Native uses **N independent terminal instances and PTYs in one C process**, not
one terminal doing 1/N of the work or N extra host processes. Its poll loop
services all active PTYs fairly. Multiplexers retain one selected visible tab;
all other tabs still produce and parse their full workloads.

After a full untimed all-producer warmup, the harness measures:

- **Output time:** common start deadline to completion of **every terminal**,
  plus the selected tab's final host frame. Each fixture appends a cursor-position
  query after its output and DONE marker; it sends a completion datagram only
  after receiving the correct terminal response. This ordered parser barrier
  verifies hidden tabs too, not merely bytes accepted into PTY buffers. The driver
  checks every PID, generation, byte count, geometry and response, and verifies
  the selected DONE in host cells with synchronized output closed. A missing
  hidden completion fails the run. Intermediate visible frames may coalesce.
- **Total CPU:** process user+system deltas for the native host **plus all mux
  client/server threads**, excluding Python driver and workload children. The
  measured PIDs are the stable host/client/server roles, not transient startup
  or discovery helpers; required-process exits invalidate the trial rather than
  dropping CPU samples. Includes PTY I/O, parsing, multiplexer rendering and host
  marker detection; not just the mux PID. Native's denominator is the C host's CPU.
- **Key p95 (quiet):** input to parsed ACK after all output has finished, with paced
  keystrokes. This is not input latency during flooding.

All displayed performance values are **cost / native cost**: native is **1.00×**,
and **higher is worse** for every column. Values are medians of paired per-trial
ratios at each pane count. Run order rotates across trials/counts; every case uses
a fresh process/session. Startup, compilation and warmup are excluded. Tiny smoke
runs verify the protocol, not performance; noisy native denominators can amplify
ratios. CPU sampling also spans the short common-start wait and completion
notification delivery. No benchmark driver subtraction or significance claim is made.

This is a headless terminal-processing comparison, **not the Ghostty application,
GPU rendering, physical display latency, or a parser-only in-memory ceiling**.
Producer scheduling and PTY transport affect wall time. This is a finite,
concurrently started workload, not an indefinitely sustained flood: faster panes
can finish before slower panes. Quiet-input latency is a separate phase, not a
claim about responsiveness during output. The host does not paint pixels, and
multiplexers need not render every intermediate input state.

Raw samples, process CPU snapshots, artifact hashes, versions and methodology are
saved to a unique `.tools/bench/run-*.json` by default (`--output` accepts a fresh
path). Raw JSON keeps absolute measurements for audit/reanalysis; the displayed
comparison uses only native-relative multiples. Failed runs are marked failed
and never summarized as successful comparisons. The shared per-user lock excludes
other tpane harness runs, not unrelated load. Run serially on an idle machine.

The all-active workload uses report schema **2**. Earlier schema-1 four-way runs
used one active producer and idle background tabs; their rankings do **not**
answer the all-active question and must not be compared directly with these runs.

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
python3 scripts/benchmark-programs.py --binary target/release/tpane \
  --trials 3 --output programs.json
python3 scripts/summarize-benchmark.py quiet.json visible.json
python3 scripts/summarize-busy.py busy8.json busy24.json
```

The current visible-output fixture varies request IDs across 1,024 log lines per block, incorporating the formerly separate changing-output experiment. It no longer emits a block of identical lines. Do not compare its throughput directly with historical repeated-line measurements. Exact historical reproduction requires the archived source/harness and workload details. Binary and harness hashes, raw samples, environment, and CPU/memory counters belong with each new result. Automated correctness and manual host validation remain separate: see [VALIDATION.md](VALIDATION.md).

## Historical evidence

Detailed investigations, raw JSON, profiles, invalid observations, patches, variant builders, and diagnostic scripts are preserved in git at **`3c9b585`**; **`8c143c6`** is the earlier checkpoint before the hidden-I/O investigation. They are deliberately removed from the working tree to keep the checkout compact.

```sh
# Read an archived investigation without restoring obsolete files.
git show 3c9b585:docs/IO_EXECUTION.md
git show 3c9b585:docs/HIDDEN_IO.md

# List or extract the detailed evidence outside this checkout.
git ls-tree -r --name-only 3c9b585 docs/performance
mkdir -p /tmp/tpane-performance-history
git archive 3c9b585 docs/performance | tar -x -C /tmp/tpane-performance-history

# Recover the matching sources, harnesses and variant builders when needed.
git worktree add --detach ../tpane-history 3c9b585
```

Key archived paths: `docs/performance/readiness/final/` for the tables, `docs/performance/hidden-io/` for the rejected yield experiment, and the earlier `frame-interval/`, `native-render-diagnosis/`, `tokio-native/`, and `async-pty/` investigations. Old capped or flawed-reader results are historical only. Invalid stale-binary and thread-CPU-unit artifacts remain labeled in the archive and are excluded from conclusions; use separate build targets when reproducing variants to avoid stale artifact reuse.
