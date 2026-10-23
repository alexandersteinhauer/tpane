# tpane

A minimal Rust terminal multiplexer for macOS, running inside Ghostty. It supports nested sessions with a keyboard-driven explorer, independent terminal state and scrollback, and shell job control. The active terminal uses the entire viewport, with no persistent bars. No server, detach/reattach, persistence, or splits.

The repository is a Cargo workspace:

- **`crates/tpane-core`**, a library: the Ghostty emulator wrapper, PTY and transport, process watching, input encoding from its own event types, the renderer (VT output or frames of styled runs), panes and selection, and **headless terminals** that a host such as a web server drives without any real terminal. It does not depend on Crossterm and builds on macOS and Linux.
- **`tpane`** (`src/`), the macOS app on top: Crossterm host I/O, the explorer, tab and command keys, the session tree and the main loop. It uses Tokio with an independent I/O worker.

Daily-use hardening remains incomplete: [manual Ghostty/Neovim validation and intermittent PTY resize findings](docs/VALIDATION.md) are still open.

## Build and run

Requires macOS, Xcode Command Line Tools, Git, and Rust through rustup. Rust **1.98.1** (edition **2024**) and `Cargo.lock` pin the Rust build.

```sh
./scripts/bootstrap-zig.sh
./scripts/cargo.sh build --release --locked
./target/release/tpane
```

Bootstrap installs checksum-verified **Zig 0.16.0** into ignored `.tools/` (macOS arm64/x86_64, Linux x86_64/aarch64), without changing your system installation. Zig builds Ghostty, not Rust. The first build needs network access for Cargo, Ghostty source, and Zig dependencies. `libghostty-vt` is pinned to libghostty-rs commit `d2036baeb605d31d09a9e2ba6d0c923e04c34d54` (unreleased; crates.io 0.2.2 still uses an older Ghostty), whose native build pins Ghostty to `3425025e585a3403340d4dc6d65132f42e05e605`. Ghostty requires a Zig with the same major.minor version as its `minimum_zig_version`, so Zig 0.17 cannot build it.

## Using tpane-core

```toml
tpane-core = { git = "…", features = ["serde"] }
```

Zig 0.16.0 is needed to build (it compiles Ghostty into a static library); nothing extra is needed where the binary runs. The library's dependencies are semver ranges (`tokio = "1"` with `rt`, `net`, `sync`, `time`, `macros`; `libc`, `anyhow`, `portable-pty`, optional `serde`); only `libghostty-vt` is pinned to a git revision, because no crates.io release has the Ghostty API it needs.

```rust
use tpane_core::{InputEvent, Key, KeyEvent, Size, Terminal, TerminalEvent, TerminalOptions};

// In a multi-thread Tokio runtime:
let options = TerminalOptions::new("/bin/bash", "/home/me/project", Size::new(100, 30));
let (terminal, mut events) = Terminal::spawn(options).await?;
let mut viewer = terminal.attach()?;           // first frame is full
terminal.input(InputEvent::Key(KeyEvent::text('l')))?;
terminal.input(InputEvent::Key(KeyEvent::text('s')))?;
terminal.input(InputEvent::Key(KeyEvent::press(Key::Enter)))?;
while let Some(frame) = viewer.next().await {    // then incremental frames
    send_to_browser(serde_json::to_string(&*frame)?);
}
let text = terminal.screen_text().await?;       // or scrollback_text()
match events.recv().await {
    Some(TerminalEvent::Copy(text)) => { /* selection to the clipboard */ }
    Some(TerminalEvent::Exited(status)) => { /* the screen stays readable */ }
    _ => {}
}
```

A `Frame` holds the changed rows as runs of text with resolved styles (RGB or default colours, attributes, underline kind and colour) and exact cell widths, plus cursor, title, the modes a viewer needs (mouse tracking, bracketed paste, alternate screen, focus reporting), default colours and the scrollbar. `ScreenModel` is a reference viewer that applies frames. Each terminal has one owner thread for its `!Send` Ghostty state; see [DESIGN.md](docs/DESIGN.md#tpane-core-and-headless-terminals). Check the core on Linux with `scripts/linux-check.sh [image]` (Docker; Alpine/musl).

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
# Absolute tpane-only numbers (no native baseline, no ratios):
just bench --variants tpane
# Short correctness smoke run (not useful performance evidence):
just bench --panes 1,3 --trials 1 --mib 1 --keys 3
```

With `native` in `--variants` the summary reports costs relative to native. Without
it, the summary reports absolute output time, aggregate/per-pane throughput, CPU
split between tpane and the shared outer C host, app CPU per MiB, app CPU as a
fraction of one core, and quiet key p95.

### Real-program workloads

`just bench` is a synthetic flood. Real terminal programs have very different
shapes: full-screen repaints, per-keystroke redraws, incremental log appends,
progress bars, color churn, and wide text. `scripts/benchmark-programs.py`
runs a scripted set of real programs over a raw PTY host and reports tpane CPU
(task and per thread), host bytes, frames, per-key frame latency, and wall time:

```sh
python3 scripts/benchmark-programs.py --binary target/release/tpane \
    --trials 3 --output programs.json
python3 scripts/benchmark-programs.py --binary old/tpane --binary new/tpane \
    --scenarios nvim-scroll,lazygit-nav,tail-f --trials 3
```

Scenarios: `cat-flood`, `seq-flood`, `less-page`, `nvim-scroll`, `nvim-typing`,
`fzf-query`, `lazygit-nav`, `htop-nav`, `tail-f`, `progress-bar`,
`style-churn`, `wide-emoji`, `git-log-flood`, `tmux-nested`. Programs are
external and unpinned: the JSON records their versions, and binaries should
only be compared on the same machine, same program versions, and interleaved
trials. See [PERFORMANCE.md](docs/PERFORMANCE.md) for measured results.

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
./scripts/cargo.sh fmt --all --check
./scripts/cargo.sh test --workspace --all-features --locked
./scripts/cargo.sh clippy --workspace --all-targets --all-features --locked -- -D warnings
./scripts/linux-check.sh            # tpane-core on Linux, in Docker
python3 scripts/test-benchmark.py
python3 scripts/test-bench.py
python3 scripts/test-bench-producer.py
```

Chromeless validation passed Clippy, release build, explorer/integration coverage, and benchmark smoke checks. The existing PTY resize test failure also reproduced during a later serial unit run; it remains unresolved. See [exact validation results and the real Ghostty checklist](docs/VALIDATION.md).

## Limits

- Intermittent PTY startup/resize failures remain unresolved, including a serial resize-test reproduction. Manual validation with the actual Ghostty, shell, and Neovim configuration is required.
- The explorer uses Unicode borders and markers, and clips explicit names by grapheme/cell width. It currently supports keyboard navigation only, with no search. No graphics/image protocols, clickable hyperlink UI, or child clipboard access.
- Selection lacks word/line selection and drag-edge autoscroll. Mouse coordinates are cell-based; keyboard layout/physical-key information is incomplete.
- Unstyled cells use host defaults; indexed colors use Ghostty's library palette, not the host's imported theme configuration. Frames resolve palette colours the same way and leave defaults to the viewer.
- The app is macOS-only (`pbcopy`, Ghostty-oriented keyboard handling); `tpane-core` is cross-platform but only macOS and Linux are tested.
- Nonfatal tab/input error presentation is deferred in the chromeless UI.
- Cleanup discovers terminal-session members and descendants, but cannot guarantee termination of fully detached/reparented daemons, privileged children, or kernel-stuck processes. SIGKILL cannot restore the host or run cleanup.
