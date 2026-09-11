# Tokio-native coordinator experiment

This is a separate architectural experiment, not another PTY-adapter swap. The measurements below are historical capped-frame results. The current candidate's execution model is described in [IO_EXECUTION.md](IO_EXECUTION.md): Ghostty stays on the caller thread while one Tokio worker keeps I/O progressing during rendering. The subsequent [frame-interval removal](FRAME_INTERVAL.md) changes both implementations and rebenchmarks capped versus uncapped builds; the Tokio-native coordinator itself remains an isolated candidate.

## Architecture

- A current-thread Tokio runtime owns Ghostty state and uses `select!` over host input, pane-tagged PTY events, control events, frame completion, child exits, and actual deadlines.
- PTY output goes directly to a bounded shared event bus. Each pane has 16 credits; the shared bus holds at most 128 events. EOF/read errors follow that pane's data on the same ordered channel.
- Input queues are byte-bounded to 8 MiB and coalesce small writes into bounded chunks. Writer tasks progress autonomously; the coordinator does not scan them to flush pending input.
- No all-pane scan on each output event or idle tick. Only the identified pane is serviced. Layout changes may still visit affected panes.
- Child exits use macOS kqueue `EVFILT_PROC/NOTE_EXIT`, not periodic `try_wait` scans. Only the owned child identified by an exit event is reaped. Targeted short retries handle exit/reap ordering, and explicit deadlines bound final-output draining.
- Host rendering builds a frame in memory. A separate async writer performs nonblocking partial writes. Only one incremental frame is in flight; later terminal changes are coalesced into the next frame, never silently dropped from the diff sequence.
- Rendering is driven by dirty visible state and host-write completion. The measurements in this report used the same approximate 60 FPS limit as the controls; that cap has since been removed from both implementations.
- `EventStream` is the host-input adapter; Crossterm internally uses one blocking helper thread. Blocking process cleanup is isolated with Tokio `spawn_blocking` and tracked in a `JoinSet`. Clipboard I/O uses Tokio process/pipe APIs with a deadline.
- Ghostty parsing/render-state updates remain synchronous CPU work on the owning thread. No Ghostty object is sent to an I/O task.

Source overrides live in `experiments/tokio-native/`. `scripts/benchmark-tokio-native.sh` creates an isolated project and `.tools/perf/tpane-tokio-native`; it does not overwrite the normal release binary.

## Debugger investigation: unread host at shutdown

A new test deliberately stops consuming host output, sends input to a running child, and requests shutdown without resuming host reads. Input dispatch remained responsive, but the process initially failed to exit within the deadline.

LLDB was attached while the target was alive, with breakpoints on `exit` and `_exit`. After SIGTERM and cleanup, LLDB stopped with **only the main thread remaining**, in `libdyld`'s exit path. The relevant stack was:

```text
libdyld.dylib  dyld4::LibSystemHelpers::exit(int) const
dyld          dyld4::LibSystemHelpersWrapper::exit(int) const
dyld          start
```

In the stopped process:

```text
(lldb) expression -- (int)fcntl(1, 3, 0)
(int) = 65538
(lldb) expression -- (int)tcflush(1, 2)
(int) = 0
```

Here `3` is `F_GETFL`, `2` is `TCOFLUSH`, and `O_NONBLOCK` is `4`. The original flags had been restored. Continuing after flushing queued output allowed the test to finish immediately; the same debugger-assisted path without that intervention did not. The application/runtime shutdown was complete, but unread terminal output prevented final process exit on this host. This does not identify the exact internal kernel wait routine.

The implementation therefore first attempts an ordered, bounded frame/reset drain. Only if that fails does it discard queued host output, restore raw-input attributes, and return failure without writing another diagnostic into the blocked terminal. Normal shutdown does not discard frames. Visual restoration cannot be guaranteed when the host refuses all output.

The optional debugger path in the generated regression test is diagnostic only; passing with a debugger intervention is not counted as a passing implementation test. Set `TPANE_LLDB_TEST=1` to attach before shutdown; `TPANE_LLDB_FLUSH=1` additionally enables the explicit flush intervention. The final implementation test passes without either variable. Raw LLDB transcripts are retained in `docs/performance/tokio-native/debugger/`.

## Measurements

Use the same release build, pane counts, frame interval, input protocol, and finite-output workloads as the existing controls. Also measure multiple hidden busy producers while typing in a quiet foreground pane using `scripts/benchmark-busy.py`.

Busy throughput means producer bytes accepted by PTYs, including bounded buffers. After stopping producers, the harness separately checks each DONE marker to prove prior bytes were processed in order. It reports drain time separately. This is not physical-display throughput.

Profiles are collected separately from benchmarks. Instrumented profiles record inclusive wall time within synchronous handlers and counts of pane visits/child checks; these are not CPU attribution and nested spans must not be summed. Profiled timings are explicitly excluded from performance comparisons.

## Results

All tables below use three trials per case and medians across trials. CPU is the tpane process, including its threads; 100% is one fully occupied core. Backend and benchmark-driver CPU are not included. Input latency ends at completed PTY-frame delivery, not physical display. Quiet cases use 80 keys with seeded 0–20 ms think time; busy cases pace 80 keys across five seconds, so the two latency tables are different workloads.

### Multiple busy panes

The foreground pane responds to input while hidden panes produce output continuously. `Accepted` includes buffering; all runs additionally verified each producer's final DONE marker after stopping it.

| Coordinator | Panes / busy | Key p50 ms | Key p95 ms | App CPU % | Accepted MiB/s | CPU ms/MiB | Threads |
|---|---:|---:|---:|---:|---:|---:|---:|
| Synchronous control | 8 / 4 | 0.15 | 0.21 | 258.01 | 178.47 | 14.46 | 18 |
| Tokio, one event at a time | 8 / 4 | 0.17 | 0.25 | 77.42 | 115.51 | 6.72 | 2 |
| Tokio, batched events | 8 / 4 | 0.17 | 0.27 | 80.12 | 136.72 | 5.86 | 2 |
| Synchronous control | 24 / 8 | 0.21 | 0.27 | 277.34 | 91.07 | 30.33 | 50 |
| Tokio, one event at a time | 24 / 8 | 0.28 | 0.38 | 87.99 | 142.71 | 6.17 | 2 |
| Tokio, batched events | 24 / 8 | 0.27 | 0.38 | 88.77 | 161.56 | 5.49 | 2 |

With 24 panes / 8 producers, the batched coordinator accepts about **77% more output using 68% less tpane CPU** than the synchronous control. CPU cost per accepted MiB falls about 82%. In the smaller 8/4 case it accepts less output, but uses much less CPU. Response-time differences in this paced workload are fractions of a millisecond; this is not evidence of a universally faster UI.

Batching uses Tokio `recv_many` with a reusable buffer of at most 64 events, rather than rebuilding/polling the full `select!` set for every small PTY packet. Data remains ordered, credits remain held until handling, and control/input/frame events are serviced between bounded batches. The original single-message variant is retained via `--single-message` and its own result files.

### Quiet panes and one active producer

| Coordinator | Panes | Idle CPU % | Key p50 ms | Key p95 ms | Threads | Idle RSS MiB |
|---|---:|---:|---:|---:|---:|---:|
| Synchronous | 1 | 0.015 | 0.20 | 17.88 | 4 | 3.14 |
| Tokio-native | 1 | <0.001 | 0.19 | 17.01 | 2 | 3.44 |
| Synchronous | 8 | 0.023 | 0.12 | 18.53 | 18 | 6.31 |
| Tokio-native | 8 | <0.001 | 0.22 | 16.97 | 2 | 6.39 |
| Synchronous | 24 | 0.044 | 0.11 | 19.47 | 50 | 13.33 |
| Tokio-native | 24 | <0.001 | 0.43 | 17.21 | 2 | 12.75 |
| Synchronous | 64 | 0.098 | 0.16 | 18.13 | 130 | 29.94 |
| Tokio-native | 64 | <0.001 | 0.21 | 17.10 | 2 | 28.02 |

The p95 remains near the frame interval. Memory savings are modest despite the large thread reduction. Idle CPU rounded below 0.001% in these samples; that is a sampled result, not a claim of literally zero overhead on every machine.

For a single active 32 MiB output producer, median throughput was **140.29 / 141.76 / 151.07 MiB/s** for synchronous at 1 / 24 / 64 panes and **124.88 / 117.41 / 125.45 MiB/s** for Tokio-native. Tokio used less tpane CPU per MiB (**5.80 / 6.05 / 5.97 ms** versus **7.95 / 8.99 / 9.37 ms**), but was not faster on that workload. Small 2 MiB completion rates are also retained in the raw files, with the usual frame-boundary sensitivity.

### Profiles

In separate, explicitly instrumented 8/4 runs, the synchronous coordinator made **5,711,129 pane visits for 921,180 feed calls** (~6.2 visits/chunk) and **450 child-status checks**. The first evented coordinator made **576,678 visits for 576,678 feed calls**, with **zero child-status checks**. The runs processed different byte totals, so absolute span durations are not speed comparisons; the counts verify the architectural change.

The batched coordinator's separate profile recorded **666,431 visits for 666,431 feed calls** and **zero child-status checks**, confirming that batching did not reintroduce scans.

The profiles record inclusive wall time in synchronous handlers, not CPU attribution. Dispatch includes Feed, and Render includes BuildFrame; summing these spans would double-count work. Uninstrumented async scheduling/I/O work is not attributed. Raw profiles are retained separately from benchmark results.

## Assessment and validation boundary

This fuller architecture **does change the conclusion** from the adapter-only experiments: it provides a substantial CPU and aggregate-output benefit with several noisy panes, eliminates periodic idle work, and remains responsive when host output stalls. It does not beat the synchronous implementation on every throughput/latency metric, and three trials on one machine do not establish universal superiority.

The current implementation remains an isolated candidate. The existing 20 integration tests and the new blocked-host test pass serially; transport, process-watch and output-service tests cover ordering, credits, partial writes, cancellation, exit registration races and flag restoration. The existing platform/PTY stress findings are not claimed fixed. The blocked-host test fails against the synchronous negative control because input dispatch stops when its host write blocks.

For this original coordinator experiment, the normal release binary and production Rust/Cargo files were unchanged. The later frame-interval change also updates the synchronous main loop. Review the shutdown fallback and exercise real Ghostty/Neovim workflows before promoting this candidate. Raw data lives in `docs/performance/tokio-native/`, including the debugger evidence and synchronous negative-control failure.

## Reproduce

```sh
./scripts/benchmark-tokio-native.sh
./scripts/cargo.sh test --manifest-path .tools/perf/tokio-native/Cargo.toml \
  --target-dir target/perf-tokio-native --release --test terminal --locked
python3 scripts/benchmark.py --binary .tools/perf/tpane-tokio-native \
  --panes 1,8,24,64 --trials 3 --keys 80 --idle-seconds 2 --output native.json
python3 scripts/benchmark-busy.py --binary .tools/perf/tpane-tokio-native \
  --panes 24 --busy 8 --trials 3 --seconds 5 --keys 80 --output native-busy.json
```

Use `scripts/benchmark-tokio-native.sh --single-message` to reproduce the pre-batching coordinator. Use `target/release/tpane` for the synchronous control. Use a fresh prefix with `scripts/profile-instrumented.py --variant native --panes 8 --busy 4 --outputprefix ...` for optional profiling; it retains the instrumented source snapshot and marks its results non-comparable. On this machine Apple's `python3` shim injects the default SDKROOT, so the profile builds explicitly used `SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX15.4.sdk` to preserve the existing Zig SDK workaround.
