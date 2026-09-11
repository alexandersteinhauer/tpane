# tpane

A minimal Rust terminal multiplexer for macOS, running inside Ghostty. It supports nested sessions with a keyboard-driven explorer, independent terminal state and scrollback, and shell job control. The active terminal uses the entire viewport, with no persistent bars. No server, detach/reattach, persistence, or splits.

The application in `src/` uses Tokio with an independent I/O worker. This is the sole implementation, not an optional experiment. Daily-use hardening remains incomplete: [manual Ghostty/Neovim validation and intermittent PTY resize findings](docs/VALIDATION.md) are still open.

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
- **Double-tap Option** (two clean taps within 400 ms) or press **Ctrl+Space** to open the centered session explorer. The fallback reserves Ctrl+Space/NUL rather than forwarding it to the child.
- **Up/Down** browses; **Left/Right** collapses/expands groups or moves to parent/child; **Home/End** jumps to the first/last entry. **Enter** switches to the selected session (or a group's remembered session). Browsing does not change the active terminal; **Esc** returns unchanged.
- **n** creates a sibling; **Shift+N** creates a child. Adding a child to a terminal preserves it as the first member of a new group and selects the new shell. **r** edits a name, **Enter** saves, and **Esc** cancels editing. Default labels are positional numbers only (`1`, `1.1`, `1.2`, `2`); explicit names appear beside them. Saving an empty name restores the numeric label.
- **d** closes the selected session or entire group. Foreground jobs require **Enter** confirmation or **Esc** cancellation; idle shells and their background jobs close without confirmation. Single-child groups remain; empty groups disappear.
- Completing an action dismisses the explorer. It overlays live terminal output without resizing any PTY; paste and mouse input are ignored while it is open. Hidden terminals keep running and groups remember their selection. New shells inherit the active terminal's working directory when macOS process inspection permits, otherwise the launch directory.
- **Option+1–9** selects the numbered top-level tab; **Option+0** selects tab 10. Hold Option and press successive digits to descend: **Option held, 2, 1** selects top-level tab 2, then its first nested tab. Release Option to start again at the top. Explorer entries show numbered paths in tree order, including `10` for the tenth sibling (its shortcut remains Option+0). Numbers are sibling positions, not shell IDs, and update when sessions close. Sessions beyond 10 remain accessible through the scrolling explorer.
- Outside the explorer, other keyboard input goes to the child. Ctrl-C and shell job control work through its PTY. Exiting a program returns to its shell; exiting the last shell exits tpane.
- Wheel-scroll up to 10,000 lines of history. Left-drag selects; release copies via `pbcopy`. Mouse-enabled applications receive mouse events; hold **Alt/Option** for local selection/scrolling if the host forwards that modifier.
- Paste with Ghostty's normal action; tpane honors the child's bracketed-paste mode.

### Ghostty keyboard setup

Set `macos-option-as-alt = true` in your Ghostty configuration so Option+digits arrive as Alt+digits rather than composed symbols. Remove any conflicting Ghostty Option+digit keybindings. tpane requests Kitty all-key/event reporting to detect Option releases and clean double taps; verify this with your actual Ghostty version and keyboard layout. If standalone modifier events are unavailable, use Ctrl+Space to open the explorer. Ghostty must not intercept that binding. Removing the native window titlebar is a separate Ghostty configuration choice, not something tpane controls.

On hosts that do not report standalone Option events, enter successive **Alt/Option+digits within one second** of each other; pause for one second to start again at the top. Typing another key, pasting, mouse interaction, resizing, or a tree mutation also ends the sequence. Missing tab numbers do nothing, and holding a digit does not repeatedly descend when the host reports key repeats.

## Architecture and performance

The caller of `Runtime::block_on` owns Ghostty, the pane tree, and rendering. One Tokio worker drives asynchronous PTY/host I/O independently; with Crossterm's helper, normal steady state has **three application threads**, not two threads per pane. Cleanup can use additional blocking workers.

PTY output queues and outstanding input are bounded. Host output preserves ordered incremental frames, with one frame in flight and backpressure—not a frame-rate cap. See [DESIGN.md](docs/DESIGN.md).

Promotion is an architectural tradeoff, not a claim of universal speedup: measured visible-output throughput remains below the synchronous control, and independent I/O costs more CPU than current-thread Tokio in some hidden-output workloads. See [PERFORMANCE.md](docs/PERFORMANCE.md) for measurements, benchmark commands, and archived evidence.

## Benchmark

```sh
brew install just tmux zellij   # if not already installed
just bench
# Short correctness smoke run (not useful performance evidence):
just bench --panes 1,3 --trials 1 --mib 1 --keys 3
```

Builds release tpane and a **C host linked directly to the same native libghostty
static library**, without Rust bindings. Compares **native / tpane / tmux / zellij**
with **every tab producing output concurrently**. Native uses the same number
of independent terminal instances in one C process. Output time, total CPU cost,
and explicitly labeled quiet-input p95 are reported as multiples of native:
**1.00× is native; higher is worse**. Defaults: 1/8/24 active tabs, three trials,
32 MiB of changing logs **per tab**.

Every terminal must acknowledge parsing its full workload, including hidden tabs;
finishing the selected tab alone is not enough. All four use the same headless
host; native has no multiplexer. This measures processing overhead, not GPU/display performance. Runs use isolated configs and
sessions, never your existing tmux/zellij sessions. Run on an otherwise idle
machine. Raw results go to `.tools/bench/`; see [methodology](docs/PERFORMANCE.md#native--tpane--tmux--zellij).

## Validation

```sh
./scripts/cargo.sh fmt --check
./scripts/cargo.sh test --locked
./scripts/cargo.sh clippy --all-targets --locked -- -D warnings
python3 scripts/test-benchmark.py
python3 scripts/test-bench.py
python3 scripts/test-bench-producer.py
```

Chromeless validation passed Clippy, release build, explorer/integration coverage, and benchmark smoke checks. The existing PTY resize test failure also reproduced during a later serial unit run; it remains unresolved. See [exact validation results and the real Ghostty checklist](docs/VALIDATION.md).

## Limits

- Intermittent PTY startup/resize failures remain unresolved, including a serial resize-test reproduction. Manual validation with the actual Ghostty, shell, and Neovim configuration is required.
- The explorer uses Unicode borders and markers, and clips explicit names by grapheme/cell width. It currently supports keyboard navigation only, with no search. No graphics/image protocols, clickable hyperlink UI, or child clipboard access.
- Selection lacks word/line selection and drag-edge autoscroll. Mouse coordinates are cell-based; keyboard layout/physical-key information is incomplete.
- Unstyled cells use host defaults; indexed colors use Ghostty's library palette, not the host's imported theme configuration.
- Nonfatal tab/input error presentation is deferred in the chromeless UI.
- Cleanup discovers terminal-session members and descendants, but cannot guarantee termination of fully detached/reparented daemons, privileged children, or kernel-stuck processes. SIGKILL cannot restore the host or run cleanup.
