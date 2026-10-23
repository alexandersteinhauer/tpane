# Design

## Scope and ownership

One foreground macOS application manages nested tabs, PTYs, terminal emulation, and a terminal-cell UI inside Ghostty. Shells and programs are child processes with independent sessions/process groups for job control. There is no background server, persistence, detach/reattach, or split layout.

The workspace has two crates. `crates/tpane-core` is the terminal core: `engine.rs` (Ghostty wrapper), `pty.rs`/`process.rs` (PTY, login shell, process-tree cleanup), `transport.rs` and `bus.rs` (PTY I/O tasks and their channels), `process_watch.rs`, `event.rs`/`input.rs` (host-independent input events and their encoding), `render.rs`/`frame.rs` (renderer and frames), `pane.rs`, `selection.rs`, and `headless.rs`. It has no Crossterm dependency and makes no assumption about stdin/stdout. `src/` is the app: Crossterm host I/O (`host.rs`, `host_input.rs`, `output.rs`), the conversion of Crossterm events into core events (`convert.rs`), `pbcopy` (`clipboard.rs`), the explorer, tab and command keys, the tree, and the main loop. The app is the independent-I/O Tokio implementation; historical synchronous and alternative async designs are in git history, not maintained variants (see [PERFORMANCE.md](PERFORMANCE.md)).

- `main.rs` calls `Runtime::block_on(run())` on a multi-thread runtime configured with **one worker** (`runtime.rs`). The main future is not spawned.
- Ghostty objects are non-`Send`/non-`Sync`. All parsing, rendering, input encoding, selection, and pane-tree mutations stay on the calling thread. A multi-thread runtime does not imply moving the UI onto its worker.
- Send-only PTY, host input/output, and process-notification tasks run independently on the Tokio worker. They can progress while the caller synchronously parses or constructs a frame.
- Normal steady state has three application threads: caller, Tokio I/O worker, and Crossterm's input helper. This excludes child processes and additional blocking workers used for cleanup.

## Event and I/O flow

`transport.rs` uses nonblocking PTY descriptors registered with `AsyncFd`. Each pane has async reader/writer tasks, not dedicated blocking I/O threads. Native PTY allocation is serialized within the process because concurrent macOS allocation has failed in isolated tests; this is not a fix for the unresolved shell resize stress failure.

Output carries stable pane IDs through a shared **128-event** bounded channel. Each reader has **16 credits** and coalesces consecutive reads into one event: it drains until the **32 KiB** buffer is full, readiness is exhausted, or a **64-read** budget is hit, then publishes. One credit therefore bounds up to 32 KiB, so queued bytes per pane are bounded by 16 events. Each read writes directly into the event's spare capacity, so there is no per-read allocation or copy, and the buffer is never zeroed. Event buffers are recycled: a consumer's drop returns the buffer to a small per-pane pool (two buffers), so steady-state output allocates nothing. Credits remain held until the coordinator consumes the bytes. Idle panes do not reserve shared queue slots. EOF and read errors follow data on the same FIFO so final output cannot be overtaken by lifecycle handling. These bounds constrain queued transport output, not total terminal/scrollback memory.

The coordinator receives up to 64 PTY events in a batch and cooperatively yields after 64 processed events or approximately 2 ms. Readers yield once per published event (not per successful read); measured 24-tab output time regressed from 3.1 s to 4.4 s when a yield was added per read, because the worker re-entered the reactor far more often. These are fairness budgets, not strict wall-clock latency guarantees or a frame interval. Input competes with PTY work; control/lifecycle events are routed separately without all-pane idle polling.

`host_input.rs` owns Crossterm `EventStream` in a separate task and forwards into a **128-event** channel. Crossterm uses the level-triggered `/dev/tty` backend to preserve simultaneous resize and keyboard readiness. Unrelated PTY wakeups do not repoll its reader mutex on the UI caller.

Outstanding child input, including an in-flight write, is limited to **8 MiB per pane**. Exceeding the limit reports an error rather than allocating indefinitely. A stopped child or large paste does not synchronously block the UI. Terminal replies are encoded locally and sent through the same bounded writer; they are not forwarded as host queries.

Process exit uses kqueue notifications on macOS and pidfds on Linux (polling `waitid(WNOWAIT)` where `pidfd_open` is unavailable). Per-pane drain deadlines and UI notices use targeted timers rather than periodic scans. Low-volume completion/control events use a separate unbounded channel; bulk PTY bytes and host input do not.

## Rendering and backpressure

`engine.rs` wraps the pinned Ghostty terminal engine; `render.rs` turns its render state into output. Parsing and rendering remain synchronous on the UI caller.

The renderer is one dirty-row walk with pluggable sinks. The walk decides what changed and how each cell looks (row and cursor dirtiness, the style-id cache, wide cells and spacer tails, graphemes, erased-cell backgrounds, reverse video, selection); a sink only encodes. The VT sink produces the host-terminal bytes described below, unchanged by the split (the reference-renderer differential test still compares them). The frame sink produces `Frame`s for viewers that draw cells themselves; see [frames](#frames).

- Hidden terminals consume output and update independent state without repainting the selected terminal.
- Switching tabs forces a complete redraw even if the selected terminal's cached render state was clean.
- Dirty rows repaint from row-level flags: one styled/grapheme/selection query per row picks between a shared base style, a style-id cache, and row-local selection ranges, instead of reading style, fg, bg, and selection state for every cell. Grapheme-free rows encode the raw codepoint directly, and wide spacer tails are inferred from the preceding wide cell. Because rows are absolutely positioned, the pen style carries across rows, so a repeated style is emitted once per frame rather than once per row. A test-only copy of the previous renderer remains for differential tests.
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
- New login shells inherit the active terminal's working directory through process inspection (libproc on macOS, `/proc` on Linux) when possible, otherwise the launch directory.
- All terminals, including hidden ones, use the full host geometry. Opening, browsing, and dismissing the explorer never resize PTYs. While open, child output is still parsed and rendered beneath the overlay; App commits synchronized output only after overlay composition. Dismissal forces a complete terminal repaint. Modal keys and their releases, paste, and mouse input do not leak into children; focus reporting treats the overlay as loss of terminal focus. Flush in-band resize replies even without child output.
- `tab_keys.rs` consumes Option+digits before child input encoding. Sibling positions 1–9 (0 for 10) select from the root, then descend one level per press while Option is held. Kitty all-key/event reporting supplies modifier releases; hosts without standalone modifier events use a one-second inter-digit timeout. Repeats/releases are consumed, and context changes reset navigation. Explorer numbering uses the same tree order.
- Outside the explorer, other keyboard input, bracketed paste, and requested mouse reporting follow child terminal modes. Alt/Option requests local selection/scrolling. Drag release returns the selected text from the pane; the app copies it through `pbcopy`. Child clipboard requests are not propagated.
- Input reaches panes as core events (`tpane_core::event`): keys named like DOM `KeyboardEvent.code` (Ghostty's key set) with the produced text, unshifted character, modifiers and press/repeat/release; mouse button, action, cell and modifiers; paste; focus; resize. `convert.rs` reconstructs physical keys, Shift and unshifted characters from Crossterm's logical characters for ASCII, exactly as the encoder previously did; its tests keep the old Crossterm-level encodings.

## Lifecycle and readiness boundary

Subtree process cleanup runs off the UI caller and is joined before application exit. Normal exit, errors, panic unwinding, SIGHUP, SIGTERM, SIGINT, and SIGQUIT attempt host restoration and child cleanup; raw keyboard Ctrl-C still goes to the child PTY. No unprivileged macOS process-container API guarantees cleanup of already detached/reparented processes; SIGKILL cannot execute restoration.

Nested tabs are implemented, but daily-use hardening is not complete. Automated coverage, unresolved parallel PTY stress behavior, and required real Ghostty/Neovim checks are tracked in [VALIDATION.md](VALIDATION.md).

## tpane-core and headless terminals

### Frames

A `Frame` has a sequence number, the size, whether it is full, the changed rows (all rows when full), a per-frame style table, the cursor (viewport position, visible, shape, blinking), the title, modes (mouse tracking off/X10/normal/button/any, bracketed paste, alternate screen, focus reporting), the terminal's default colours if a program set them, and the scrollbar. Each listed row is complete: runs cover it from column 0, and cells after the last run are default blanks (trailing default blanks are omitted, which keeps frames small). A run is either single-codepoint cells of one width (1, or 2 for wide characters) or one grapheme cluster with its width, so viewers need no width or grapheme tables. Colours are resolved to RGB or left as the default; `inverse` already combines reverse video and the selection.

An incremental frame `n` applies to the state after frame `n - 1`; a full frame `n` is the state at `n`. `ScreenModel` is the reference viewer and rejects gaps. Tests compare it with Ghostty's own formatter over styled, wide, combining, scrolled and erased content, and incremental results with fresh full frames.

### Headless terminals

`Terminal::spawn` starts a command (by default `shell -l`, with `TERM=xterm-256color`, the enclosing emulator's variables removed and the given environment on top) in a PTY at a size, on its own **owner thread**. The owner holds the terminal's `!Send` state (engine, renderer, encoders, selection) and runs `Handle::block_on` on the host's **multi-thread** Tokio runtime, so the PTY reader/writer and exit watcher are Send tasks on the runtime's workers, as in the app. An idle terminal is one parked thread; a host runs many terminals in one process.

The `Terminal` handle is `Send + Sync` and cheap to clone; requests go over an unbounded channel and are handled in order: input events, resize, attach, screen or scrollback text (Ghostty's formatter), shutdown. Events come back on a channel: copy requests (a completed mouse selection, instead of `pbcopy`), bells, non-fatal errors, and the exit status. After the shell exits its process tree is cleaned up but the screen stays readable and viewable; dropping the last handle (or `shutdown`) ends the owner thread and terminates the shell if it is still running.

Viewers pull frames. A new viewer's first frame is full. Frames are rendered only when a viewer is waiting and something changed, so output floods coalesce into few frames; a frame rendered for one viewer is queued for the others. A viewer more than 16 frames behind, or one that dropped a pending `next()`, gets a full frame next instead of a gap. A full frame for one viewer first renders pending changes for everyone, so it is exactly the state at the current sequence number. While a program holds synchronized output (mode 2026), frames wait for at most 500 ms.

### Linux

The core builds and passes its tests on Linux (checked on Alpine/musl aarch64 in Docker with `scripts/linux-check.sh`). Process inspection reads `/proc/<pid>/stat` and `/proc/<pid>/cwd`; the foreground-job check uses `tcgetpgrp`, as on macOS. Process-tree cleanup is the same best-effort session/descendant discovery; it does not use cgroups.
