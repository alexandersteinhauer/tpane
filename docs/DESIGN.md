# Design

## Scope and ownership

One foreground macOS application manages nested tabs, PTYs, terminal emulation, and a terminal-cell UI inside Ghostty. Shells and programs are child processes with independent sessions/process groups for job control. There is no background server, persistence, detach/reattach, or split layout.

`src/` contains the sole application: the independent-I/O Tokio implementation. Historical synchronous and alternative async designs are available in git history, not maintained variants (see [PERFORMANCE.md](PERFORMANCE.md)).

- `main.rs` calls `Runtime::block_on(run())` on a multi-thread runtime configured with **one worker** (`runtime.rs`). The main future is not spawned.
- Ghostty objects are non-`Send`/non-`Sync`. All parsing, rendering, input encoding, selection, and pane-tree mutations stay on the calling thread. A multi-thread runtime does not imply moving the UI onto its worker.
- Send-only PTY, host input/output, and process-notification tasks run independently on the Tokio worker. They can progress while the caller synchronously parses or constructs a frame.
- Normal steady state has three application threads: caller, Tokio I/O worker, and Crossterm's input helper. This excludes child processes and additional blocking workers used for cleanup.

## Event and I/O flow

`transport.rs` uses nonblocking PTY descriptors registered with `AsyncFd`. Each pane has async reader/writer tasks, not dedicated blocking I/O threads. Native PTY allocation is serialized within the process because concurrent macOS allocation has failed in isolated tests; this is not a fix for the unresolved shell resize stress failure.

Output carries stable pane IDs through a shared **128-event** bounded channel. Each reader has **16 credits**, with reads up to **16 KiB**; credits remain held until the coordinator consumes the bytes. Idle panes do not reserve shared queue slots. EOF and read errors follow data on the same FIFO so final output cannot be overtaken by lifecycle handling. These bounds constrain queued transport output, not total terminal/scrollback memory.

The coordinator receives up to 64 PTY events in a batch and cooperatively yields after 64 processed events or approximately 2 ms. Readers also yield between successful reads. These are fairness budgets, not strict wall-clock latency guarantees or a frame interval. Input competes with PTY work; control/lifecycle events are routed separately without all-pane idle polling.

`host_input.rs` owns Crossterm `EventStream` in a separate task and forwards into a **128-event** channel. Crossterm uses the level-triggered `/dev/tty` backend to preserve simultaneous resize and keyboard readiness. Unrelated PTY wakeups do not repoll its reader mutex on the UI caller.

Outstanding child input, including an in-flight write, is limited to **8 MiB per pane**. Exceeding the limit reports an error rather than allocating indefinitely. A stopped child or large paste does not synchronously block the UI. Terminal replies are encoded locally and sent through the same bounded writer; they are not forwarded as host queries.

Process exit uses macOS kqueue notifications. Per-pane drain deadlines and UI notices use targeted timers rather than periodic scans. Low-volume completion/control events use a separate unbounded channel; bulk PTY bytes and host input do not.

## Rendering and backpressure

`engine.rs` wraps the pinned Ghostty terminal engine; `render.rs` constructs incremental host-terminal output. Parsing and rendering remain synchronous on the UI caller.

- Hidden terminals consume output and update independent state without repainting the selected terminal.
- Switching tabs forces a complete redraw even if the selected terminal's cached render state was clean.
- `output.rs` asynchronously writes ordered frames, handling partial writes and WouldBlock. The coordinator permits **one incremental frame in flight** and draws pending visible changes after its completion.
- No frame-rate cap, delayed-flush timer, or dropping/replacing accepted incremental frames. An empty draw does not count as a presented frame.
- Slow host output applies backpressure while input, signals, and I/O tasks can still progress. Shutdown attempts an ordered terminal reset with a bounded wait, then falls back to best-effort restoration if the host remains blocked.

Independent I/O improves measured visible output over current-thread Tokio but does not remove synchronous parsing/rendering cost or cross-thread handoff overhead. Further optimization remains unproven.

## Nested tabs and input

`tree.rs` is a pure tree of terminal IDs and groups with selected children; a separate map owns terminal state and lifecycle resources.

- The active terminal occupies the full viewport without persistent chrome. `explorer.rs` provides a centered tree overlay: arrows browse without switching, Enter activates, Esc dismisses, n adds a sibling, N adds a child, r renames, and d closes. Wrapping a terminal preserves it as the first child and creates a selected second shell. Completed actions dismiss the overlay.
- Groups remember their selected children and hidden terminals keep running. Explorer labels are sibling-number paths (`1`, `1.1`, `1.2`, `2`, …, `10`), optionally followed by explicit names; internal IDs and automatic directory names are never shown. Unicode rounded borders, disclosure arrows, and a single active-leaf dot keep the overlay minimal. Names are sanitized and clipped on grapheme boundaries by display-cell width. All branches initially expand; the list scrolls to keep the selected entry visible.
- `command_keys.rs` detects two clean Option press/release cycles within 400 ms, invalidating gestures on other input, overlapping modifiers, long holds, repeats, or context changes. Ctrl+Space/NUL is the portable fallback. Option events otherwise continue to `tab_keys.rs`.
- Closing a subtree containing foreground jobs requires confirmation. Removing siblings preserves selected identity; removing the selected child selects the next sibling, falling back to the previous. Single-child groups remain; empty groups disappear. The last shell's exit ends tpane.
- New login shells inherit the active terminal's working directory through macOS inspection when possible, otherwise the launch directory.
- All terminals, including hidden ones, use the full host geometry. Opening, browsing, and dismissing the explorer never resize PTYs. While open, child output is still parsed and rendered beneath the overlay; App commits synchronized output only after overlay composition. Dismissal forces a complete terminal repaint. Modal keys and their releases, paste, and mouse input do not leak into children; focus reporting treats the overlay as loss of terminal focus. Flush in-band resize replies even without child output.
- `tab_keys.rs` consumes Option+digits before child input encoding. Sibling positions 1–9 (0 for 10) select from the root, then descend one level per press while Option is held. Kitty all-key/event reporting supplies modifier releases; hosts without standalone modifier events use a one-second inter-digit timeout. Repeats/releases are consumed, and context changes reset navigation. Explorer numbering uses the same tree order.
- Outside the explorer, other keyboard input, bracketed paste, and requested mouse reporting follow child terminal modes. Alt/Option requests local selection/scrolling. Drag release copies through `pbcopy`; child clipboard requests are not propagated.

## Lifecycle and readiness boundary

Subtree process cleanup runs off the UI caller and is joined before application exit. Normal exit, errors, panic unwinding, SIGHUP, SIGTERM, SIGINT, and SIGQUIT attempt host restoration and child cleanup; raw keyboard Ctrl-C still goes to the child PTY. No unprivileged macOS process-container API guarantees cleanup of already detached/reparented processes; SIGKILL cannot execute restoration.

Nested tabs are implemented, but daily-use hardening is not complete. Automated coverage, unresolved parallel PTY stress behavior, and required real Ghostty/Neovim checks are tracked in [VALIDATION.md](VALIDATION.md).
