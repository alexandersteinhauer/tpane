# tpane

A minimal Rust terminal multiplexer for macOS, running inside Ghostty. It supports nested numbered tabs with mouse and keyboard navigation, independent terminal state and scrollback, and shell job control. No server, detach/reattach, persistence, or splits.

The application in `src/` uses Tokio with an independent I/O worker. This is the sole implementation, not an optional experiment. Daily-use hardening remains incomplete: [manual Ghostty/Neovim validation and parallel PTY stress findings](docs/VALIDATION.md) are still open.

## Build and run

Requires macOS, Xcode Command Line Tools, Git, and Rust through rustup. Rust **1.98.1** (edition **2024**) and `Cargo.lock` pin the Rust build.

```sh
./scripts/bootstrap-zig.sh
./scripts/cargo.sh build --release --locked
./target/release/tpane
```

Bootstrap installs checksum-verified **Zig 0.15.2** into ignored `.tools/`, without changing your system installation. Zig builds Ghostty, not Rust. The first build needs network access for Cargo, Ghostty source, and Zig dependencies. `libghostty-vt` is pinned to **0.2.1**, whose native build pins Ghostty to `a887df42c56f6de86c0fe6da9c4eeca37931e083`.

On Apple Silicon, Zig 0.15.2 cannot use the arm64e-only top-level stubs in the macOS 26.5 SDK. `scripts/cargo.sh` detects this and selects an installed macOS 15.4 SDK. If necessary, install a compatible SDK and select it explicitly:

```sh
SDKROOT=/path/to/MacOSX15.4.sdk ./scripts/cargo.sh build --release --locked
```

## Use

- Starts `$SHELL` as a login shell, falling back to `/bin/zsh`. Override with `tpane --shell /bin/zsh`.
- Click `[+]` on the empty bar to preserve the current terminal as tab one and open a selected second shell. Each child has its own empty bar for deeper nesting; group bars add siblings.
- Click ancestor tabs to switch branches. Hidden terminals keep running; groups remember their selection. New shells inherit the active terminal's working directory when macOS process inspection permits, otherwise the launch directory.
- Click `[x]` to close a subtree. Foreground jobs require `[Cancel]` / `[Close]` confirmation; idle shells and their background jobs close without confirmation. Single-child groups retain their bar; empty groups disappear.
- **Option+1–9** selects the numbered top-level tab; **Option+0** selects tab 10. Hold Option and press successive digits to descend: **Option held, 2, 1** selects top-level tab 2, then its first nested tab. Release Option to start again at the top. Tab labels show only their number, without shell/group names: 1–9, then 0 for tab 10. Numbers are sibling positions, not shell IDs, and update when tabs close. Tabs beyond 10 remain mouse-accessible.
- Other keyboard input goes to the child. Ctrl-C and shell job control work through its PTY. Exiting a program returns to its shell; exiting the last shell exits tpane.
- Wheel-scroll up to 10,000 lines of history. Left-drag selects; release copies via `pbcopy`. Mouse-enabled applications receive mouse events; hold **Alt/Option** for local selection/scrolling if the host forwards that modifier.
- Paste with Ghostty's normal action; tpane honors the child's bracketed-paste mode.

### Ghostty keyboard setup

Set `macos-option-as-alt = true` in your Ghostty configuration so Option+digits arrive as Alt+digits rather than composed symbols. Remove any conflicting Ghostty Option+digit keybindings. tpane requests Kitty all-key/event reporting to detect Option releases; verify this with your actual Ghostty version and keyboard layout.

On hosts that do not report standalone Option events, enter successive **Alt/Option+digits within one second** of each other; pause for one second to start again at the top. Typing another key, pasting, mouse interaction, resizing, or a tree mutation also ends the sequence. Missing tab numbers do nothing, and holding a digit does not repeatedly descend when the host reports key repeats.

## Architecture and performance

The caller of `Runtime::block_on` owns Ghostty, the pane tree, and rendering. One Tokio worker drives asynchronous PTY/host I/O independently; with Crossterm's helper, normal steady state has **three application threads**, not two threads per pane. Cleanup can use additional blocking workers.

PTY output queues and outstanding input are bounded. Host output preserves ordered incremental frames, with one frame in flight and backpressure—not a frame-rate cap. See [DESIGN.md](docs/DESIGN.md).

Promotion is an architectural tradeoff, not a claim of universal speedup: measured visible-output throughput remains below the synchronous control, and independent I/O costs more CPU than current-thread Tokio in some hidden-output workloads. See [PERFORMANCE.md](docs/PERFORMANCE.md) for measurements, benchmark commands, and archived evidence.

## Validation

```sh
./scripts/cargo.sh fmt --check
./scripts/cargo.sh test --locked
./scripts/cargo.sh clippy --all-targets --locked -- -D warnings
python3 scripts/test-benchmark.py
```

The promoted checkout passed Clippy, 56 unit + 21 integration tests serially, and the release integration suite. The default parallel run reproduced the known PTY resize failure; it remains unresolved. See [validation results and the real Ghostty checklist](docs/VALIDATION.md).

## Limits

- Parallel PTY startup/resize stress failures remain unresolved. Manual validation with the actual Ghostty, shell, and Neovim configuration is required.
- No tab overflow management, graphics/image protocols, clickable hyperlink UI, or child clipboard access. Deep bars are clipped to preserve at least one terminal row.
- Selection lacks word/line selection and drag-edge autoscroll. Mouse coordinates are cell-based; keyboard layout/physical-key information is incomplete.
- Unstyled cells use host defaults; indexed colors use Ghostty's library palette, not the host's imported theme configuration.
- Tab/input errors appear in a dismissible top-bar notice rather than terminating unrelated terminals.
- Cleanup discovers terminal-session members and descendants, but cannot guarantee termination of fully detached/reparented daemons, privileged children, or kernel-stuck processes. SIGKILL cannot restore the host or run cleanup.
