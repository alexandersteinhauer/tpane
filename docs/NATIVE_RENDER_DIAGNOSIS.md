# Why uncapped Tokio-native visible output regressed

Follow-up implementation and validation: [IO_EXECUTION.md](IO_EXECUTION.md). Direct readiness did not solve the regression; separating I/O execution from the caller-owned UI recovered much of the throughput without restoring a frame cap.

## Finding

The primary regression is excessive synchronous frame construction on the current-thread runtime, not an idle render spin and not a sudden increase in terminal parsing cost. The old frame deadline masked an inefficient render cadence. Input-stream polling also causes avoidable mutex contention.

This investigation leaves both normal application binaries and their source implementations unchanged. Diagnostic variants are isolated; none is promoted as a fix.

## Direct measurements

Separate instrumented single-pane runs each processed the same 32 MiB workload plus startup/input protocol bytes (33,558,646 bytes offered to the engine). Counts and spans cover the whole process lifetime, including startup and cleanup. These runs are not normal performance comparisons.

| Counter / inclusive synchronous-handler wall time | Capped | Uncapped |
|---|---:|---:|
| Render calls | 27 | 6,639 |
| Frame construction | 1.90 ms | 453.54 ms |
| Feed calls | 32,858 | 32,849 |
| Terminal feed time | 34.54 ms | 37.27 ms |
| Child-status checks | 0 | 0 |

Frame construction consumes about twelve times the terminal-feed wall time in the uncapped profile. It performs repeated cell/style/grapheme extraction and ANSI frame construction on the same thread that runs the I/O tasks. Feed time remains similar while render work explodes. Inclusive spans are not CPU attribution; nested Render/BuildFrame and Dispatch/Feed spans must not be summed.

For the uncapped run:

- 32,483 of 32,666 `recv_many` calls returned **one event** (99.44%); the other 183 returned two.
- 6,422 of 6,639 render attempts began while the PTY event channel already contained data (96.73%).
- The configured maximum batch size of 64 was therefore not an actual 64-event batch in this workload.

The corrected uninstrumented frame-interval experiment independently recorded about 20.3 MiB of host output uncapped versus 0.047 MiB capped, and app CPU per child MiB increased from 5.85 to 21.94 ms. That is consistent with the profile, not merely a timing-tail artifact.

## Code path

In `experiments/tokio-native/main.rs`, the `biased` select places the immediately-ready render arm above PTY activity. Once visible state is dirty and the preceding write completes, painting wins even if more PTY data is queued. The 64-event receiver limit only drains what is already available; it does not collect a useful work batch across reactor/task turns.

The reader in `transport.rs` explicitly yields after every successful read, and also yields on would-block/retry paths. Reads are approximately 1 KiB in these observations. Readiness, producer scheduling, and these yields limit what is available when the coordinator receives. The explicit successful-read yield alone is **not** a verified explanation: the counterfactual below did not improve batching.

Unlike the synchronous implementation's independent PTY reader threads, native I/O tasks cannot run while the owning thread constructs a frame. One-frame-in-flight output bounds write backlog, but does not prevent premature CPU work constructing the next frame. Rendering therefore needs useful work coalescing, not just a channel capacity or an async write API.

## Sampled input overhead

A separate macOS `sample` run captured 1,710 main-thread stack observations while the original uncapped binary handled a longer visible-output workload:

- A subtree containing 308 observations (18.0%) was in `Crossterm EventStream::poll_next` through `parking_lot::RawMutex::lock_slow`; 284 reached `swtch_pri` via `sched_yield`.
- The input helper thread was waiting in `select` throughout the sample.
- Numerous main-thread samples were in Ghostty render-cell/style/grapheme getters; relatively few were in terminal feed.

These are sampled stacks, **not an 18% CPU attribution**. They include scheduling and waits. Crossterm 0.29's stream polls its global reader with a zero timeout before checking its wake-task flag; `poll_internal` uses `try_lock_for`. Re-polling this future on unrelated PTY wakeups repeatedly encounters the helper's reader lock.

## Counterfactuals, including unsuccessful ones

The same corrected harness ran three uninstrumented single-pane 32 MiB trials per variant. All variants retained no frame interval and one host frame in flight. These are narrow diagnostic experiments, not fully validated application replacements.

| Isolated variant | MiB/s | App CPU ms/MiB | Key p95 ms | Host output MiB |
|---|---:|---:|---:|---:|
| Current uncapped control | 33.71 | 24.35 | 0.23 | 20.13 |
| Explicit reader yield only after 64 reads | 33.41 | 25.71 | 0.19 | 22.56 |
| Drain already-queued data before painting, up to 64 events | 35.20 | 23.90 | 0.22 | 20.07 |
| EventStream in its own Tokio task, bounded input channel | 38.02 | 22.10 | 0.21 | 21.45 |
| Queued-data priority plus input task | 38.94 | 21.98 | 0.23 | 21.01 |

The bounded-reader profile still averaged only 1.007 events per receive and made 7,388 renders. Thus merely changing that yield does not solve the cadence. Draining the channel's currently queued data is also insufficient: channel emptiness does not mean the PTY producer/readiness pipeline is drained. Isolating EventStream modestly helps in this experiment, but the large render regression remains.

The current control's absolute throughput differs from the earlier ~40 MiB/s median; the comparisons here use a fresh control and the same workload/options. Small differences are not claimed universal. The negative experiments are retained rather than described as successful fixes.

## Follow-up: is batching actually the right fix?

The user requested further investigation rather than committing to a batching redesign. No application changes were made in this follow-up.

### Actual frame content

A separate diagnostic hashes each complete wire segment containing synchronized-output begin/end markers during the 32 MiB finite-output phase. Standalone extra end markers are excluded. Hashing runs are explicitly not benchmarks.

| Payload | Implementation | Frames | Distinct wire frames | Consecutive identical frames |
|---|---|---:|---:|---:|
| Repeated log line | Native | 6,487 | 4 | 6,483 |
| Repeated log line | Synchronous | 1,992 | 2 | 1,990 |
| Varied request numbers | Native | 6,509 | 130 | 0 |
| Varied request numbers | Synchronous | 1,808 | 68 | 0 |

The original fixture repeats exactly the same 64-byte log line. After the viewport fills, scrolling frequently reconstructs the same visible output. This exposes redundant rendering, but is not representative of every changing display.

The varied fixture changes the six-digit request number on every line within each 64 KiB block, keeping line length, byte total and ASCII structure the same. The block repeats, explaining the finite number of distinct frames; consecutive repaints now differ. In three separate uninstrumented trials, native still achieved **39.75 MiB/s** versus synchronous **122.12 MiB/s**, with app costs of **22.00 versus 11.03 CPU ms/MiB**. Thus eliminating identical consecutive frames alone is not a general explanation or solution for the throughput gap.

### Pinned renderer API review

Read-only inspection of libghostty-vt 0.2.1 and its pinned Ghostty commit `a887df42c56f6de86c0fe6da9c4eeca37931e083` found:

- Ghostty explicitly allows dirty false positives. Rebuilt render rows are marked dirty without comparing their visual content to the previous row. Cursor/selection changes can invalidate more than the changed text.
- Our renderer reconstructs every cell of each dirty row. A typical non-tail cell involves eight FFI operations, including iteration, wide-cell lookup, style, colors, selection and grapheme extraction. Style equality only suppresses redundant ANSI style emission; it does not avoid those lookups.
- The C API can batch fields for **one cell**, but does not publicly export a whole row/cell slice. The safe Rust wrapper does not expose those multi-get handles. Optional missing fields can terminate a multi-get early, so replacing calls blindly is unsafe.
- Selection bounds can be fetched once per row through the safe API. More substantial shared-renderer improvements require careful treatment of erased-cell backgrounds, palette/default changes, reverse mode, wide/combining text and cursor state.
- Raw cell/style IDs are not sufficient persistent visual cache keys: styles are page-local and may be remapped, while grapheme extensions live separately. A comparison after fully encoding a row saves host bytes but not its construction cost.
- The VT formatter is not a drop-in pane renderer: default bounds include scrollback, it uses line-oriented output, lacks the pane's cursor offset, and selection restricts content rather than highlighting it.

The full review is retained as `docs/performance/native-render-diagnosis/renderer-api-review.md`. The measured 454 ms includes snapshot update, cell extraction, encoding and buffer output; it does not prove that the FFI boundary alone dominates.

### What this changes

There are two distinct areas to evaluate before selecting a root fix:

1. **Work scheduling:** paint at a meaningful terminal-state boundary rather than at an arbitrary small channel-message boundary. Merely enlarging a receive limit or yielding is already shown insufficient.
2. **Shared renderer cost:** avoid unnecessary reconstruction and improve extraction/encoding of genuinely changed content. Exact-frame deduplication after encoding would leave most CPU work intact and would favor only the repeated-line fixture.

Any candidate needs varied output and editor-like screen changes, not just identical log flooding. It must also retain uncapped interactive latency, fairness and bounded host backpressure.

Tokio's documented guarantees support this distinction: `select!` branches share a task/thread, and `yield_now` does not guarantee another task or the I/O driver runs before resumption. Exa was used to locate the official docs; the behavior was checked against pinned 1.53.1 sources. Links are in `research-sources.md` alongside the raw evidence. Documentation does not establish which architecture will benchmark best.

## Evidence boundary

Verified: rendering far too many intermediate states is the main new work, the input adapter suffers repeated lock contention, and these simple local changes do not recover capped throughput. There is no observed idle-spin regression.

Still unresolved: a validated timer-free scheduling/coalescing change that reduces unnecessary frames while preserving immediate interactive responses, input fairness, multiple busy-pane fairness, and bounded host backpressure. No claim is made that moving the select arm or changing one yield fixes all of that.

Raw sample output, source hashes, instrumentation, profiles and benchmark observations are in `docs/performance/native-render-diagnosis/`. The retained helpers are `experiments/profiling/sample-visible.py` and `experiments/profiling/diagnose-visible.py`; the latter snapshots the already-generated native project, refuses existing snapshot directories, and keeps build products under `.tools/perf/visible-diagnosis/` and `target/perf-profile/`. On this machine invoke builds with the existing explicit macOS 15.4 SDKROOT workaround.
