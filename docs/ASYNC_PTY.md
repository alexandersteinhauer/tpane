# Async PTY driver experiment

## Question

Does replacing the per-pane blocking PTY reader/writer threads improve the already optimized event-driven multiplexer? This extends the orchestration-only Tokio experiment in `PERFORMANCE.md`.

The default application is unchanged. `scripts/benchmark-async-pty.sh` generates an isolated project under `.tools/perf/async-pty`, builds into `target/perf-async-pty`, and writes `.tools/perf/tpane-async-pty`. The driver source is retained in `experiments/async-pty/io_worker.rs`.

## Implementation

- Tokio 1.53.1, current-thread runtime with I/O and timers enabled.
- Each PTY master is duplicated with CLOEXEC, made nonblocking, and registered with `AsyncFd`. Reader and writer tasks share the descriptor.
- PTY I/O uses readiness guards around real nonblocking `read`/`write` syscalls, including partial writes, EINTR/EAGAIN, EOF, and PTY read-side EIO.
- Output remains bounded to 16 chunks of at most 16 KiB. Total queued/in-flight input remains bounded to 8 MiB. Backpressure never blocks the runtime thread.
- Dropping a pane aborts both I/O tasks. Their descriptor references are released when Tokio processes cancellation. Existing process cleanup remains responsible for terminating shells/jobs.
- Ghostty terminal state stays on the main thread. Its non-`Send` types are never captured by I/O tasks.
- The host-input reader remains one blocking thread. Process-cleanup threads may exist transiently while tabs close; this is not a thread-free application.
- The main loop yields cooperatively when its work budget is exhausted, so PTY tasks can progress even during sustained input/output.

Setting `O_NONBLOCK` on a duplicated descriptor changes the shared file-description flags. The prototype must not also use the blocking PTY adapters on that master. The builder adjusts the generated pane integration accordingly, without changing the production `Session` API.

## Validation

Seven driver tests cover large/partial transfers, byte ordering, bounded queues, EOF/write errors, cancellation, and real macOS PTY hangup. Additional black-box tests send a 1 MiB paste while the child deliberately does not read, interact with tabs/resize during the stall, verify the complete payload and trailer, and close/reopen subtrees with blocked input while checking process cleanup.

A parallel integration run passed 19 of 20 tests but failed a nested-layout assertion after the shell printed `child setpgid ... Operation not permitted` inside its size marker. The terminal reported the correct `21 80` dimensions, but the warning split the expected marker. This has not been diagnosed and is not counted as a passing test. In a separate serialized comparison, both drivers passed all 20 integration tests, including the two new blocked-input tests. Serialization is a test condition, not a fix for the warning. The pre-existing platform/PTY stress findings in `VALIDATION.md` also remain open.

## Measurement method

Compare three release binaries: optimized synchronous workers, Tokio orchestration with those same workers, and the actual `AsyncFd` PTY driver. Workloads and main-loop processing/rendering limits are held constant, except for the cooperative yield needed to run PTY tasks on the same thread.

Primary cases use 1, 8, 24, and 64 panes, three trials each, 80 paced key samples, a 100-key burst, two seconds idle, and 2 MiB of output. Sustained cases use 1, 24, and 64 panes with three trials of 32 MiB output, 20 key samples, and 0.5 seconds idle. Only the selected pane produces workload output; the others are idle.

The existing benchmark records input-to-completed-PTY-frame latency, idle/workload CPU, output throughput, and thread counts. It now also records virtual address size alongside resident size. Memory values are phase-boundary snapshots of the tpane process, not peaks or total system/backend memory; virtual size is not physical RAM consumption.

Run variants serially on an otherwise idle machine. Three trials per case are exploratory measurements, not proof of a universally faster architecture. Physical Ghostty rendering, actual Neovim configuration, power state, and external load are outside the synthetic benchmark.

## Results

Values below are medians across three trials. Latency columns are medians of per-trial percentiles, not pooled percentiles. CPU percentages use one core as 100%. These are fresh control measurements, not copied from the earlier event-loop comparison.

### Responsiveness and idle resources

| Driver | Panes | Key p50 ms | Key p95 ms | Idle CPU % | Threads | Idle RSS MiB |
|---|---:|---:|---:|---:|---:|---:|
| Synchronous workers | 1 | 1.35 | 16.80 | 0.014 | 4 | 3.16 |
| Tokio + workers | 1 | 2.57 | 17.64 | 0.028 | 4 | 3.19 |
| AsyncFd | 1 | 2.19 | 16.22 | 0.024 | 2 | 3.33 |
| Synchronous workers | 8 | 1.12 | 16.63 | 0.024 | 18 | 6.31 |
| Tokio + workers | 8 | 2.61 | 17.64 | 0.040 | 18 | 6.47 |
| AsyncFd | 8 | 1.82 | 16.20 | 0.040 | 2 | 6.27 |
| Synchronous workers | 24 | 1.07 | 16.86 | 0.044 | 50 | 13.31 |
| Tokio + workers | 24 | 2.61 | 17.43 | 0.055 | 50 | 13.31 |
| AsyncFd | 24 | 2.34 | 16.17 | 0.058 | 2 | 12.69 |
| Synchronous workers | 64 | 1.22 | 16.83 | 0.107 | 130 | 29.92 |
| Tokio + workers | 64 | 1.90 | 18.19 | 0.112 | 130 | 30.06 |
| AsyncFd | 64 | 2.19 | 16.77 | 0.119 | 2 | 28.17 |

The thread reduction is real, but it does **not** translate into equally dramatic physical-memory savings: about 0.62 MiB RSS at 24 panes and 1.75 MiB at 64. Virtual address size is about 280 MiB lower at 64 panes; that is reserved address space, not 280 MiB of saved RAM. At one pane, the async variant uses slightly more RSS.

### Sustained output

| Driver | Panes | Median MiB/s | Trial range MiB/s | tpane CPU ms/MiB |
|---|---:|---:|---:|---:|
| Synchronous workers | 1 | 102.92 | 48.54–140.76 | 9.43 |
| Tokio + workers | 1 | 151.19 | 131.44–151.80 | 7.86 |
| AsyncFd | 1 | 123.38 | 122.89–131.92 | 5.33 |
| Synchronous workers | 24 | 123.83 | 65.58–130.91 | 9.93 |
| Tokio + workers | 24 | 131.34 | 122.58–139.25 | 8.95 |
| AsyncFd | 24 | 109.55 | 109.30–116.11 | 6.43 |
| Synchronous workers | 64 | 103.48 | 57.13–109.22 | 11.48 |
| Tokio + workers | 64 | 148.79 | 130.93–179.60 | 9.33 |
| AsyncFd | 64 | 82.26 | 78.91–89.88 | 9.47 |

AsyncFd reduces tpane CPU cost per MiB strongly at 1 and 24 panes, including against the same-runtime Tokio+workers control. It does not consistently improve throughput; median throughput is lower at 24 and 64 panes. The synchronous throughput samples are noisy, so the table includes their ranges rather than presenting a precise universal speed ratio. Backend/driver CPU is excluded, so these are not whole-workload energy measurements.

Small 2 MiB output cases remain sensitive to final-frame timing; their complete data is retained but is not used as evidence of sustained throughput.

### Batching experiment

The first driver yields after each I/O operation to ensure cancellation and UI progress. I also tested batching ready reads/writes until a 64 KiB budget (still bounded by channel capacity), with writes capped to 16 KiB per syscall. It did not improve the measured outcome:

| AsyncFd variant | Panes | Sustained MiB/s | CPU ms/MiB |
|---|---:|---:|---:|
| Per-operation yield | 1 | 123.38 | 5.33 |
| Batched | 1 | 109.47 | 6.14 |
| Per-operation yield | 24 | 109.55 | 6.43 |
| Batched | 24 | 98.73 | 7.09 |
| Per-operation yield | 64 | 82.26 | 9.47 |
| Batched | 64 | 77.12 | 9.78 |

The batched variant passed the same seven driver tests and 20 serialized integration tests. Its primary and sustained measurements are retained separately, and `--batched` rebuilds it using `experiments/async-pty/batched.patch`. The default experimental build retains the simpler per-operation-yield implementation. This comparison does not establish why batching was worse; it prevents adopting a scheduling change on intuition alone.

## Decision

This decision applies to the adapter-only experiment. The later [Tokio-native coordinator experiment](TOKIO_NATIVE.md) replaces the polling-oriented coordinator too and shows different results under multi-pane load; this report is not an async performance ceiling.

**Async PTYs are compatible with libghostty and useful for reducing thread count, but this prototype is not a clear responsiveness or throughput upgrade over the optimized synchronous default.** Keep the default unchanged for now.

The result is a resource/CPU trade-off: drastically fewer threads, modest RSS savings, and lower tpane bulk-output CPU at moderate pane counts, but no consistent speed gain and unresolved parallel-test warnings. A production switch would need additional hardening and representative real-workflow measurements, particularly simultaneous noisy panes and a slow host renderer. Host stdout rendering remains synchronous in both variants and can block the main thread.

Raw observations are in `docs/performance/async-pty/`. `asyncfd*.json` identifies the PTY-task implementations; `tokio-workers*.json` is the orchestration-only control. No production Rust files, production Cargo dependencies, or normal release executable were changed for this experiment.

## Reproduce

```sh
./scripts/cargo.sh build --release --locked
./scripts/benchmark-tokio.sh
./scripts/benchmark-async-pty.sh

./scripts/cargo.sh test --manifest-path .tools/perf/async-pty/Cargo.toml \
  --target-dir target/perf-async-pty --release --bin tpane io_worker::tests --locked
./scripts/cargo.sh test --manifest-path .tools/perf/async-pty/Cargo.toml \
  --target-dir target/perf-async-pty --release --test terminal --locked
```

For each binary, run these workloads serially (use a different output path each time):

```sh
python3 scripts/benchmark.py --binary .tools/perf/tpane-async-pty \
  --panes 1,8,24,64 --trials 3 --keys 80 --idle-seconds 2 --output asyncfd.json
python3 scripts/benchmark.py --binary .tools/perf/tpane-async-pty \
  --panes 1,24,64 --trials 3 --keys 20 --idle-seconds .5 \
  --output-bytes 33554432 --output asyncfd-sustained.json
python3 scripts/summarize-async-pty.py asyncfd.json asyncfd-sustained.json
```

Use `target/release/tpane` for the synchronous control and `.tools/perf/tpane-tokio-wake` for Tokio+workers. `./scripts/benchmark-async-pty.sh --batched` produces `.tools/perf/tpane-async-pty-batched` for the scheduling comparison.
