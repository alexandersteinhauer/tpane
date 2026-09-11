# Terminal and nested-tab validation

The independent-I/O Tokio application is now the sole implementation in `src/`. Promotion alone does not establish daily-use readiness. Historical results are archived at `691c180` (earlier checkpoint `cbeb68a`); see [PERFORMANCE.md](PERFORMANCE.md).

## Promotion checks (2026-09-11)

On macOS 26.6.2 / Apple M5 Pro, Rust 1.98.1, in the async-only working tree based on `691c180`:

- Formatting, locked release build, and Clippy for all targets with `-D warnings`: passed.
- `cargo test --locked -- --test-threads=1`: **56 unit + 21 integration tests passed**.
- All **21 integration tests also passed against the release binary** via `TPANE_TEST_BINARY`, and in a separate default-parallel debug integration run.
- Benchmark driver regression tests: **5 passed**; the consolidated changing-output harness smoke test passed.
- Default parallel `cargo test --locked`: **55 unit tests passed, one failed** (`pty::tests::resize_reaches_slave`, old `24 80` instead of `37 101`). Cargo stopped before integration tests. This is the same unresolved symptom documented before promotion, not a clean full-suite pass.
- LLDB ran the parallel unit binary with a `rust_panic` breakpoint; all 56 passed without hitting it. The failure did not reproduce under the debugger, so no causal backtrace was obtained. The later backtrace command failed because the process had already exited normally; this is not evidence of a fix.

Local logs and benchmark observations are under ignored `.tools/validation/async-promotion/`. These runs do not replace the manual checklist or resolve parallel PTY stress.

## Automated checks

```sh
./scripts/cargo.sh fmt --check
./scripts/cargo.sh build --release --locked
./scripts/cargo.sh test --locked
./scripts/cargo.sh clippy --all-targets --locked -- -D warnings
python3 scripts/test-benchmark.py
```

Coverage includes terminal emulation/rendering, pane-tree behavior, input modes and selection, PTY transport, bounded input/output, frame ordering/backpressure, lifecycle/restoration, blocked-host shutdown, Neovim resize, and I/O progress while the Ghostty-owning caller is occupied. Black-box tests run the actual binary inside a PTY; they do not modify the real clipboard. Record command, revision, environment, and failures rather than treating historical test counts as a current pass.

The suite does not use a real Ghostty window. The unchecked manual items below remain required with the actual shell/editor configuration.

## Open stress-test finding

Historical repeated parallel PTY unit-test runs—and the promotion check above—intermittently reported the old `24 80` size instead of `37 101` in `pty::tests::resize_reaches_slave`, immediately after login-shell startup. An initial PTY-write `EIO` was also observed once. These have not been traced to a verified cause. Serial and interactive resize integration runs pass, but that does not resolve the parallel failure.

Separately, an isolated no-shell allocator test reproduced concurrent macOS PTY allocation failures (`Unknown error: -6`). Serializing allocation eliminated those failures in that experiment, so allocation is guarded within each process. This did **not** eliminate the intermittent shell resize-test failure and must not be presented as its fix.

Before declaring daily-use hardening complete, capture master/slave window sizes and shell signal activity across the failing resize to distinguish an application race from shell or platform behavior. Reproduce with repeated `./scripts/cargo.sh test --bin tpane pty::tests --locked` runs; do not replace this with reruns until green.

## Manual: build

- [ ] Build using the commands in `README.md` and launch `./target/release/tpane` inside Ghostty.
- [ ] Confirm the expected login shell starts in the launch directory.
- [ ] Repeat with the actual shell configuration used for work.

## Shell and input

- [ ] Type ASCII, accented characters, CJK, combining characters, and emoji; check editing and cursor alignment.
- [ ] Exercise arrow keys, Home/End, Delete, Backspace, Ctrl-key editing, Option-key behavior, and function keys.
- [ ] Paste multiline text at the prompt; confirm bracketed-paste behavior matches the shell's normal behavior.
- [ ] Run `sleep 60`, interrupt with Ctrl-C, and confirm the same shell remains usable.
- [ ] Run `sleep 60`, suspend with Ctrl-Z, then use `bg`, `jobs`, and `fg`; interrupt it.
- [ ] Exit a command with a nonzero status and confirm the shell remains open.

## Rendering and resizing

- [ ] Run the normal editor (for example, Neovim), resize repeatedly while idle without typing, and confirm it redraws immediately. Then enter insert mode, edit, resize again, and exit back to the original shell screen.
- [ ] Test a mouse-enabled TUI with clicking, dragging, and scrolling enabled in that application.
- [ ] Resize down to a very small window and back; check cursor position and wrapping.
- [ ] Run `stty size` after resizing and confirm it matches the available terminal dimensions.
- [ ] Check 256 colors, truecolor, bold, italic, underline, inverse text, and cursor shape changes.
- [ ] Run `yes`, resize while it runs, then interrupt it and enter another command. Input and shutdown should remain responsive.
- [ ] Repeat with several hidden output producers while editing in Neovim; check input, tab switching, and output isolation. Automated paced-input benchmarks do not establish visible-flood latency.

## History and clipboard

- [ ] Produce more than a screen of output and scroll up/down using the wheel.
- [ ] Type while scrolled up and confirm the viewport returns to the active prompt.
- [ ] Drag-select text in the current screen and in scrollback; release and paste it elsewhere.
- [ ] Select across a soft-wrapped line and confirm copying does not insert a spurious newline.
- [ ] Confirm wide/combining characters are selected and copied correctly.
- [ ] In a mouse-capturing application, check that normal mouse input reaches it and Alt/Option-drag uses local selection.

Clipboard tests intentionally replace the current clipboard: save anything important first.

## Shutdown

- [ ] Use `exit` and confirm Ghostty returns to the outer shell with normal echo, cursor, mouse, paste, and wrapping behavior.
- [ ] Start a background job inside tpane, then exit tpane; confirm the managed job is gone.
- [ ] Send SIGTERM to tpane from another terminal and confirm cleanup and host restoration.
- [ ] Close the Ghostty window containing tpane and confirm the managed shell/jobs terminate.

## Nested tabs

- [ ] Click the initial `[+]`. Confirm the existing shell is preserved as tab one and a new shell is selected as tab two.
- [ ] Change directory, add a tab, and check that the new shell starts in that directory.
- [ ] Use each child terminal's empty bar to create two or more nesting levels. Confirm every visible ancestor bar can switch branches.
- [ ] Select a nested child, switch away through an ancestor, and switch back. The same descendant should be selected, with its screen and scrollback intact.
- [ ] Run Neovim in one branch and a noisy command in another. Switch repeatedly; verify no mixed frames or lost input.
- [ ] Resize while Neovim is visible and while it is hidden, then select it again. It should use the available content size without requiring a keystroke.
- [ ] Use a mouse-enabled TUI under multiple bars. Clicking tabs must not reach the TUI; clicks inside its content must land on the correct row.
- [ ] Select and copy text under multiple bars. Check drag/release near bar boundaries and verify copying still uses the correct terminal.
- [ ] Close a subtree containing a foreground job. Cancel once, verify every child is intact, then confirm and check that only that subtree's processes terminate.
- [ ] Exit a hidden shell and confirm its tab disappears without disturbing the selected branch.
- [ ] Close down to one child and verify the group retains its bar; close its last child and verify empty groups disappear.
- [ ] Make the window shorter than the active nesting depth, then enlarge it. Terminal dimensions must remain valid and the bars should return.

Record failures with the Ghostty version, shell/editor version, terminal dimensions, exact command/input, and whether they reproduce outside tpane. See `README.md` for the documented process-cleanup limits.
