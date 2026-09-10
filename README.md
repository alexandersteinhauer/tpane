# tpane

A minimal Rust terminal multiplexer for macOS, running inside Ghostty.

**Current milestone: part 1, a single terminal.** No tabs, detach/reattach, persistence, or async runtime yet. See [the design](docs/DESIGN.md) for the next milestones.

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

- The initial terminal fills the window and starts `$SHELL` as a login shell, falling back to `/bin/zsh`. It inherits the application's working directory.
- Override the shell with `tpane --shell /bin/zsh`.
- Keyboard input goes to the shell/application. There are no multiplexer keyboard shortcuts.
- Exit a foreground program normally to return to its shell. Use `exit` (or the shell's EOF behavior) to close tpane.
- Mouse wheel scrolls the terminal's own history, retained for up to 10,000 lines.
- Drag with the left mouse button to select text. Releasing copies the selection to the macOS clipboard using `pbcopy`.
- Mouse events go to applications that request them. Hold **Alt/Option** to use tpane's local selection/scrolling instead; the host must forward that modifier and mouse event.
- Paste using Ghostty's normal paste action. Bracketed paste is encoded according to the child application's mode.

The application handles resize, alternate screens, cursor style, Unicode, colors, terminal queries, and mode-aware keyboard/mouse input. Background jobs remain in the terminal's session; Ctrl-C and shell job control are not intercepted by tpane.

## Architecture

- The main thread owns Ghostty terminal state, selection, input encoding, and incremental rendering.
- A blocking PTY reader feeds a bounded channel. The main loop parses a bounded amount of output per iteration and renders at up to approximately 60 FPS.
- A blocking PTY writer keeps a stopped program or a large paste from blocking rendering and shutdown. Outstanding input is limited to 8 MiB; exceeding the limit produces an error rather than unbounded allocation.
- Terminal replies are handled locally, not forwarded as queries to the host.
- Normal exit, errors, Rust panic unwinding, SIGHUP, SIGTERM, SIGINT, and SIGQUIT restore the host terminal and initiate child cleanup. Raw keyboard Ctrl-C still reaches the child PTY.

## Validation

```sh
./scripts/cargo.sh fmt --check
./scripts/cargo.sh test --locked
./scripts/cargo.sh clippy --all-targets --locked -- -D warnings
```

Automated tests exercise emulation, rendering, input modes, scrollback selection, PTY behavior, bounded input, and process cleanup. They do not modify the real clipboard.

Before treating this as daily-use ready, run the [manual Ghostty checklist](docs/VALIDATION.md) with your actual shell/editor configuration. Automated PTY tests are not a substitute for testing the real host's input and rendering behavior.

## Prototype limitations

- No graphics/image protocols, clickable hyperlink UI, or propagation of application clipboard requests. Clipboard reads from child applications are not allowed.
- Crossterm reports cell-based mouse positions, not exact pixel positions. Keyboard layout/physical-key information is also incomplete; extended input needs testing with your layout.
- Selection is basic drag selection; no word/line selection or drag-edge autoscroll yet. Use the wheel to reach history before selecting.
- Host theme defaults are used for unstyled cells; Ghostty's library palette is used for indexed colors. The host application's theme configuration is not imported.
- Cleanup discovers terminal-session members and descendants across job-control process groups. macOS has no unprivileged process-container API: a daemon that fully detaches and reparents before discovery, privileged children, and kernel-stuck processes cannot be guaranteed to terminate. SIGKILL of tpane cannot run cleanup or restore the host terminal.
