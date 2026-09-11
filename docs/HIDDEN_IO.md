# Hidden-output I/O overhead investigation

Checkpoint before investigation: `cbeb68a`. Application implementations and the production binary remain unchanged during these diagnostic experiments.

## Question

In the final 24-pane / 8-hidden-producer comparison, moving asynchronous I/O to an independent Tokio worker preserved roughly 158 MiB/s accepted throughput but raised tpane CPU from about 88% to 126% of one core. Visible rendering is minimal here. Determine whether the extra cost is I/O, task scheduling, or cross-thread coordination rather than assuming another renderer problem.

## Methods

- Existing busy harness: 24 panes, eight hidden producers, a quiet foreground responder, five seconds, exact final DONE verification.
- Separate macOS `sample` runs: two seconds at 1 ms. Stack observations include waiting/descheduling and are not CPU percentages. Timings from these runs are excluded from benchmarks.
- Separate libproc boundary snapshots: task CPU, each stable thread's CPU, process syscall/context-switch counters. Thread IDs are libproc handles, not sample's kernel thread identifiers.
- `proc_threadinfo` times on this host are nanoseconds; `proc_taskinfo` times are Mach ticks. The initial wrong unit assumption was caught by comparing summed thread deltas with process deltas and is retained under `invalid/`. Corrected thread sums match process CPU within sampling skew.
- Ordinary uninstrumented comparisons are separate from both diagnostics. No driver-overhead subtraction or mixing of profiled timings into benchmark medians.

## Initial thread observations

One diagnostic run per implementation; not statistical performance medians.

| Implementation / thread | User CPU s | System CPU s | Total CPU s |
|---|---:|---:|---:|
| Current-thread: main | 1.420 | 2.875 | 4.295 |
| Current-thread: input helper | 0.00034 | 0.00129 | 0.00162 |
| Independent-worker: main | 1.635 | 0.613 | 2.248 |
| Independent-worker: tpane-io | 0.688 | 3.482 | 4.170 |
| Independent-worker: input helper | 0.00037 | 0.00150 | 0.00187 |

Process totals were 4.297 and 6.420 CPU-seconds. Accepted throughput was 152.55 and 161.65 MiB/s respectively, so absolute costs must not be compared as equal byte totals.

Across the five-second windows:

| Process counter | Current-thread | Independent worker |
|---|---:|---:|
| Unix syscalls | 950,238 | 1,525,116 |
| Mach syscalls | 1,037,568 | 161 |
| Context switches | 66,501 | 214,263 |

The independent worker eliminates the previously observed repeated main-thread Crossterm mutex spinning, but introduces substantial cross-thread wake/park activity. Samples place much of the main thread in Tokio's condition-variable park path; the I/O worker frequently appears in `read` and `kevent`. The main thread's active stacks primarily feed Ghostty. These observations locate the work but do not assign an exact CPU percentage to each function.

## Isolated hypothesis test

`yield64` changes **only** the PTY reader's explicit yield after a successful read: one yield after every 64 reads instead of after every read. Per-pane credits, shared channel capacity, buffer allocation, runtime threads, rendering and would-block handling stay unchanged. This bounds the amount of work between explicit yields; it is not a timer or a frame interval. Tokio's own cooperative budget and genuine I/O/backpressure waits still apply.

This test previously failed to help the current-thread visible-output implementation. It must be retested in the independent-worker hidden-output context rather than assumed to have the same effect.

The source snapshot/hash manifest and exact patch are retained alongside the observations. The candidate builds into an isolated target directory to avoid the earlier shared-target stale-artifact problem. It is not installed as the default application.

## Yield-frequency result

Three uninstrumented trials per case, medians. Both use the independent I/O worker; only the successful-read yield frequency differs.

| Case | Variant | MiB/s | App CPU % | App CPU ms/MiB | Key p95 ms |
|---|---|---:|---:|---:|---:|
| Hidden 24 / 8 | Committed control | 165.54 | 130.40 | 7.88 | 0.38 |
| Hidden 24 / 8 | Yield every 64 reads | 170.67 | 140.47 | 8.23 | 0.35 |
| Hidden 8 / 4 | Committed control | 163.87 | 128.74 | 7.86 | 0.29 |
| Hidden 8 / 4 | Yield every 64 reads | 183.32 | 141.71 | 7.86 | 0.25 |
| Visible changing 32 MiB, 1 pane | Committed control | 120.69 | — | 12.22 | 0.19 |
| Visible changing 32 MiB, 1 pane | Yield every 64 reads | 109.34 | — | 13.40 | 0.17 |

This is not an efficiency win. At 24/8, throughput increases about 3% but CPU rises about 8%; CPU per accepted byte worsens. The 8/4 case processes more bytes for proportionally more CPU, while visible-output throughput falls. Eight transport tests passed, but that does not justify promoting the change. The candidate remains isolated.

In the candidate's separate counter run, Unix syscalls increased to 2,210,382 and context switches to 242,976. The follow-up diagnostic below counts read attempts returning WouldBlock to test whether fewer explicit yields merely cause more unsuccessful read attempts. Counter runs and uninstrumented trials are separate observations, not mixed medians.

## Why fewer yields did not help

Separate source-instrumented runs counted real read syscalls and receiver batch sizes. These counts/spans cover the entire application lifetime, not precisely the five-second steady window; startup/drain/shutdown are included. Normalize by actual bytes read because the runs processed different totals.

| Counter | Committed control | Yield every 64 reads |
|---|---:|---:|
| Bytes read | 856,366,216 | 924,693,640 |
| Read syscalls | 1,066,339 | 1,789,182 |
| WouldBlock results | 229,992 | 886,110 |
| Read syscalls / MiB | 1,305.68 | 2,028.88 |
| WouldBlock results / MiB | 281.61 | 1,004.82 |
| Mean receive batch size | 2.108 | 1.825 |
| Single-event receive batches | 46.48% | 52.01% |

Removing most successful-read yields causes **3.57× as many WouldBlock results per MiB**, while the receiver gets smaller—not larger—batches. This is direct evidence of extra unsuccessful I/O attempts, not merely a scheduler-cost hypothesis. Tokio correctly clears readiness on WouldBlock; the experiment does not justify clearing it after a short successful read or adding sleeps.

The committed control's profile recorded 836,346 feed calls and just 130 render calls. Inclusive frame-construction wall time was **5.83 ms**, versus **1,122 ms** in terminal feed over the whole run. Rendering is not the hidden-workload bottleneck. These are inclusive synchronous wall spans, not CPU attribution; do not sum overlapping spans or use profile timings as ordinary benchmark results.

## Conclusion

The independent-worker architecture trades an important visible-output improvement for substantial kernel/coordination work when hidden output keeps both sides active. The committed reader/receiver exchanges mostly tiny units of work. Thread counters and stack samples locate the extra work in I/O, wake/park paths and parsing rather than idle spinning or repainting.

The specific proposed yield reduction is rejected: it does not improve CPU efficiency and increases unsuccessful reads. No application source or default binary was changed by this investigation. Eight isolated transport tests passed, but the experimental variant is not a validated replacement.

A next investigation should target publication/allocation granularity and handoffs across the I/O/UI boundary while preserving prompt partial-output delivery and per-pane fairness—not blindly reducing yields, changing reactor settings, or adding workers. This remains a hypothesis to test, not a claimed fix. New investigation files remain separate from checkpoint `cbeb68a`.

## Reproduce diagnostics

```sh
python3 scripts/profile-tpane.py --binary BINARY --panes 24 --busy 8 \
  --output NEW_SAMPLE_PATH.txt
python3 scripts/profile-hidden-io.py --binary BINARY --output NEW_COUNTERS_PATH.json
```

The counter helper refuses existing outputs and validates CPU units/scope before publishing its summary. Raw data: `docs/performance/hidden-io/`.
