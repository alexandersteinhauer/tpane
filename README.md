# tpane

A minimal Rust terminal multiplexer for macOS, running inside Ghostty.

**Current milestone: part 2, nested tabs.** No detach/reattach, persistence, splits, or async runtime. See [the design](docs/DESIGN.md) for the remaining daily-use validation.

## Build and run

Requires macOS, Xcode Command Line Tools, Git, and Rust installed through rustup. The project pins **stable Rust 1.98.1**, edition **2024**.

```sh
./scripts/bootstrap-zig.sh
./scripts/cargo.sh build --release --locked
./target/release/tpane
```

The bootstrap script installs checksum-verified **Zig 0.15.2** into the ignored `.tools/` directory; it does not change your system installation. Zig builds Ghostty, not the Rust application. The first build needs network access to fetch Cargo dependencies, the pinned Ghostty source, and Ghostty's Zig dependencies.

`libghostty-vt` is pinned to `0.2.1`; its native build pins Ghostty to `a887df42c56f6de86c0fe6da9c4eeca37931e083`. `Cargo.lock` pins the Rust dependency graph.

### macOS SDK compatibility

Zig 0.15.2 cannot link against the arm64e-only top-level stubs in the macOS 26.5 SDK on Apple Silicon. `scripts/cargo.sh` detects that case and uses the installed macOS 15.4 SDK. It also makes Zig honor `SDKROOT` during SDK discovery. If that older SDK is unavailable, install a compatible SDK and set its path explicitly:

```sh
SDKROOT=/path/to/MacOSX15.4.sdk ./scripts/cargo.sh build --release --locked
```

With a compatible default SDK and Zig 0.15.2 already on `PATH`, ordinary `cargo` commands also work.

## Use

- The initial terminal starts `$SHELL` as a login shell, falling back to `/bin/zsh`, beneath an empty tab bar. It inherits the application's working directory.
- Click the right-aligned `[+]` to add a tab. The first click preserves the current terminal as the first tab and opens a new shell as the selected second tab.
- Every child terminal has its own empty bar for deeper nesting. Existing group bars add siblings. Click any ancestor tab to switch; each group remembers its selection.
- New shells inherit the active terminal's working directory via macOS process inspection when available, otherwise the launch directory.
- Click `[x]` to close a tab and its entire subtree. If any terminal in the subtree has a foreground job, the top bar offers `[Cancel]` and `[Close]`. Closing an idle shell also terminates its background jobs without confirmation.
- Single-child groups keep their bar. Empty groups are removed. Initial tab labels are stable `Shell N` and `Tabs N`, rather than application-supplied titles.
- Override the shell with `tpane --shell /bin/zsh`.
- Keyboard input goes to the shell/application. There are no multiplexer keyboard shortcuts.
- Exit a foreground program normally to return to its shell. `exit` (or the shell's EOF behavior) closes that terminal's tab. Closing the final terminal exits tpane.
- Mouse wheel scrolls the terminal's own history, retained for up to 10,000 lines.
- Drag with the left mouse button to select text. Releasing copies the selection to the macOS clipboard using `pbcopy`.
- Mouse events go to applications that request them. Hold **Alt/Option** to use tpane's local selection/scrolling instead; the host must forward that modifier and mouse event.
- Paste using Ghostty's normal paste action. Bracketed paste is encoded according to the child application's mode.

The application handles resize, alternate screens, cursor style, Unicode, colors, terminal queries, and mode-aware keyboard/mouse input. Background jobs remain in the terminal's session; Ctrl-C and shell job control are not intercepted by tpane.

## Architecture

- Host events use Crossterm's level-triggered `/dev/tty` backend so simultaneous resize and keyboard readiness are not lost.
- A pure pane tree holds terminal IDs and selected children. The main thread owns each terminal's independent Ghostty state, selection, input encoding, and renderer.
- Native PTY allocation is serialized within the process to avoid concurrent macOS allocation failures; terminal I/O and child execution remain independent.
- Each terminal has a blocking PTY reader and bounded channel. A dedicated host-input reader and the PTY workers wake the main thread when work arrives. Round-robin output draining is bounded to 64 chunks or approximately 2 ms per turn so input remains responsive.
- The main loop parks when idle instead of polling every 4 ms. Changed visible state renders after each bounded work batch, with no frame-rate cap. Child status checks run at 100 ms intervals or after EOF.
- All terminals, including hidden ones, are resized according to their nesting depth. Ancestor bars are excluded from PTY dimensions; mouse coordinates are translated into terminal-local coordinates. In-band resize replies are flushed even without child output.
- Tab switching forces a complete redraw of the selected terminal, including when its cached render state was clean. Subtree process cleanup runs on worker threads and is joined before application exit.
- A blocking PTY writer keeps a stopped program or a large paste from blocking rendering and shutdown. Outstanding input is limited to 8 MiB; exceeding the limit produces an error rather than unbounded allocation.
- Terminal replies are handled locally, not forwarded as queries to the host.
- Normal exit, errors, Rust panic unwinding, SIGHUP, SIGTERM, SIGINT, and SIGQUIT restore the host terminal and initiate child cleanup. Raw keyboard Ctrl-C still reaches the child PTY.

## Performance

Both implementations now have no frame-rate cap; see the [capped/uncapped retest](docs/FRAME_INTERVAL.md). The follow-up [I/O execution investigation](docs/IO_EXECUTION.md) separates Tokio I/O progress from synchronous rendering while retaining caller-thread ownership of Ghostty.

See [the measured event-loop comparison](docs/PERFORMANCE.md) and its raw results. The optimized synchronous loop removes the tabbed version's output throttling and reduces median input latency and idle CPU in the tested workloads. A current-thread Tokio prototype showed no consistent advantage at 1–24 panes, so the production build remains synchronous.

```sh
python3 scripts/benchmark.py --binary target/release/tpane --output results.json
```

Use an otherwise idle machine and run variants serially. This measures completed PTY frames, not physical display latency. `scripts/benchmark-tokio.sh` builds an isolated optional prototype without adding Tokio to the production dependencies.

The [AsyncFd PTY comparison](docs/ASYNC_PTY.md) removes per-pane I/O threads but retains the old coordinator. The subsequent [Tokio-native coordinator](docs/TOKIO_NATIVE.md) removes pane/status polling, batches targeted events, and makes host output nonblocking. It shows substantial CPU and aggregate-output gains with multiple busy panes, though it is not faster on every workload. Both remain isolated experiments; `scripts/benchmark-tokio-native.sh` builds the full coordinator without replacing the default.

## Validation

```sh
./scripts/cargo.sh fmt --check
./scripts/cargo.sh test --locked
./scripts/cargo.sh clippy --all-targets --locked -- -D warnings
```

Automated tests exercise emulation, rendering offsets, input modes, scrollback selection, pane-tree mutations, bar hit testing, PTY behavior, bounded input, and process cleanup. Black-box tests run the actual binary inside a PTY. They do not modify the real clipboard.

Before treating this as daily-use ready, run the [manual Ghostty checklist](docs/VALIDATION.md) with your actual shell/editor configuration. Automated PTY tests are not a substitute for testing the real host's input and rendering behavior.

## Prototype limitations

- Repeated parallel PTY stress tests still expose an intermittent startup/resize failure; see the open finding in `docs/VALIDATION.md`. Daily-use hardening (part 3) is not complete.
- Tab overflow is not managed; enlarge the window when tabs become too narrow to label or close. If nesting exceeds the window height, deeper bars are clipped to preserve at least one terminal row and return when the window grows.
- Tab operations and input errors appear temporarily in the top bar; click the notice to dismiss it. They do not terminate unrelated terminals.
- No graphics/image protocols, clickable hyperlink UI, or propagation of application clipboard requests. Clipboard reads from child applications are not allowed.
- Crossterm reports cell-based mouse positions, not exact pixel positions. Keyboard layout/physical-key information is also incomplete; extended input needs testing with your layout.
- Selection is basic drag selection; no word/line selection or drag-edge autoscroll yet. Use the wheel to reach history before selecting.
- Host theme defaults are used for unstyled cells; Ghostty's library palette is used for indexed colors. The host application's theme configuration is not imported.
- Cleanup discovers terminal-session members and descendants across job-control process groups. macOS has no unprivileged process-container API: a daemon that fully detaches and reparents before discovery, privileged children, and kernel-stuck processes cannot be guaranteed to terminate. SIGKILL of tpane cannot run cleanup or restore the host terminal.
