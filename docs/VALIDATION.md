# Terminal and nested-tab validation

The independent-I/O Tokio application is now the sole implementation in `src/`. Promotion alone does not establish daily-use readiness. Historical results are archived at `3c9b585` (earlier checkpoint `8c143c6`); see [PERFORMANCE.md](PERFORMANCE.md).

## Promotion checks (2026-09-11)

On macOS 26.6.2 / Apple M5 Pro, Rust 1.98.1, in the async-only working tree based on `3c9b585`:

- Formatting, locked release build, and Clippy for all targets with `-D warnings`: passed.
- `cargo test --locked -- --test-threads=1`: **56 unit + 21 integration tests passed**.
- All **21 integration tests also passed against the release binary** via `TPANE_TEST_BINARY`, and in a separate default-parallel debug integration run.
- Benchmark driver regression tests: **5 passed**; the consolidated changing-output harness smoke test passed.
- Default parallel `cargo test --locked`: **55 unit tests passed, one failed** (`pty::tests::resize_reaches_slave`, old `24 80` instead of `37 101`). Cargo stopped before integration tests. This is the same unresolved symptom documented before promotion, not a clean full-suite pass.
- LLDB ran the parallel unit binary with a `rust_panic` breakpoint; all 56 passed without hitting it. The failure did not reproduce under the debugger, so no causal backtrace was obtained. The later backtrace command failed because the process had already exited normally; this is not evidence of a fix.

Local logs and benchmark observations are under ignored `.tools/validation/async-promotion/`. These runs do not replace the manual checklist or resolve parallel PTY stress.

## Chromeless explorer checks

On the chromeless working tree:

- Locked release build, formatting, and all-target Clippy with `-D warnings` passed.
- An initial serial suite passed **82 unit + 26 integration tests**, with the same 26 integration tests also passing against the release binary.
- A subsequent serial unit run passed **81 tests and failed `pty::tests::resize_reaches_slave`**, reporting `24 80` instead of `37 101`. The PTY implementation is unchanged; this matches the existing unresolved symptom, now observed serially as well. It was not rerun to obtain a green result.
- After adding the black-box double-Option test, the serial debug integration run passed **26 of 27 tests** (including double-Option). `first_sibling_preserves_shell_state_and_inherits_active_cwd` timed out because `/bin/sh` printed `child setpgid ... Operation not permitted` between the size marker and the correct `24 80` value. This separate shell/job-control diagnostic remains unexplained; it is not evidence of an explorer geometry mismatch. The added double-Option test also passed against the release binary; final Clippy and formatting checks passed.
- All **69 Python harness/producer tests** passed. Native/tpane smoke benchmarks passed at **1/3/12 sessions**, one trial, 1 MiB per session, three keys. These small runs verify harness operation, not performance claims.
- The numeric-label/Unicode refresh adds positional renumbering and grapheme/cell-width clipping tests; all **15 explorer unit tests** passed. Integration snapshots now assert rounded borders, disclosure arrows, selected-row markers, and a single active-leaf dot. After updating those visual assertions, all **27 integration tests**, all-target Clippy, formatting, and the locked release build passed.
- Gesture, modal navigation, viewport preservation, output composition, input isolation, and process cleanup have automated coverage. Real Ghostty double-Option behavior still requires the manual checks below.

## Core split and headless frames (2026-10-23)

On macOS 27.0 / Apple Silicon, Rust 1.98.1, Zig 0.16.0, branch `rad/53829e37`:

- Workspace formatting, all-target/all-feature Clippy with `-D warnings`, and the locked release build passed.
- `cargo test --workspace --all-features --locked`: app **54 unit + 27 integration**, core **45 unit + 11 headless end-to-end** tests passed. The VT renderer's reference differential test and all render tests are unchanged; the old Crossterm-level input encodings are kept as tests of `convert.rs`.
- Linux: `scripts/linux-check.sh rad-linux-build:aarch64` (Alpine, musl, aarch64, Rust 1.99.1) built the core and passed **45 unit + 11 headless** tests, including PTY process-tree cleanup, pidfd exit watching, `/proc` inspection, vim, resize and eight terminals in one process. glibc and x86_64 Linux were not run.
- Renderer: `render::tests::microbench_render_paths` (release, four alternating runs, 100x28 log screen, identical 3027-byte frames) measured the VT path at 22.3-24.9 µs/frame before the walk/sink split and 24.0-24.8 µs/frame after: within run-to-run noise.
- `scripts/benchmark-programs.py`, previous release binary vs this one, interleaved: two trials of cat-flood, seq-flood, less-page, nvim-scroll, style-churn, wide-emoji and progress-bar, then six trials of less-page, wide-emoji and cat-flood. tpane CPU deltas: cat-flood -0.9%, less-page +2.2%, nvim-scroll 0.0%, seq-flood +0.3%, style-churn -4.0%, progress-bar -4.7%, wide-emoji -27% (with 6-12% more, equally sized frames presented in the flood scenarios). The two-trial less-page run showed +33% on 0.05 s of CPU; six trials did not reproduce it.
- `just bench --panes 1,3 --trials 1 --mib 1 --keys 3` passed before and after; as a one-trial smoke run its ratios (tpane CPU 1.9-3.6x native) vary more between runs than between builds and are not performance evidence.

Still open for the core and headless mode:

- No real browser viewer has consumed frames yet; frames are checked against `ScreenModel`, Ghostty's formatter and fresh full frames only.
- Frames carry no hyperlinks (OSC 8), images (Kitty graphics), or mouse pointer shape; selection is the core's own (mouse with tracking off, or Alt).
- Mouse positions are cell-granular (SGR-pixel reports multiply by the cell size). Without `unshifted`, Kitty alternate keys assume a US layout.
- Linux process cleanup uses the same best-effort session/descendant discovery as macOS, not cgroups; reparented daemons can escape.
- The owner-thread-per-terminal model is untested at hundreds of terminals.

## Automated checks

```sh
./scripts/cargo.sh fmt --all --check
./scripts/cargo.sh build --release --locked
./scripts/cargo.sh test --workspace --all-features --locked
./scripts/cargo.sh clippy --workspace --all-targets --all-features --locked -- -D warnings
./scripts/linux-check.sh            # tpane-core on Linux, in Docker
python3 scripts/test-benchmark.py
```

Coverage includes terminal emulation/rendering, pane-tree behavior, input modes and selection, PTY transport, bounded input/output, frame ordering/backpressure, lifecycle/restoration, blocked-host shutdown, Neovim resize, and I/O progress while the Ghostty-owning caller is occupied. Black-box tests run the actual binary inside a PTY; they do not modify the real clipboard. Record command, revision, environment, and failures rather than treating historical test counts as a current pass.

The suite does not use a real Ghostty window. The unchecked manual items below remain required with the actual shell/editor configuration.

## Open stress-test finding

Historical repeated parallel PTY unit-test runs—and the promotion check above—intermittently reported the old `24 80` size instead of `37 101` in `pty::tests::resize_reaches_slave`, immediately after login-shell startup. An initial PTY-write `EIO` was also observed once. These have not been traced to a verified cause. Earlier serial and interactive resize integration runs passed; the chromeless validation above also reproduced the unit-test symptom serially. Neither observation resolves its cause.

Separately, an isolated no-shell allocator test reproduced concurrent macOS PTY allocation failures (`Unknown error: -6`). Serializing allocation eliminated those failures in that experiment, so allocation is guarded within each process. This did **not** eliminate the intermittent shell resize-test failure and must not be presented as its fix.

Before declaring daily-use hardening complete, capture master/slave window sizes and shell signal activity across the failing resize to distinguish an application race from shell or platform behavior. Reproduce with repeated `./scripts/cargo.sh test -p tpane-core --lib pty::tests --locked` runs; do not replace this with reruns until green.

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

## Chromeless session explorer

- [ ] Double-tap Option and separately press Ctrl+Space. Verify both open the centered explorer in the actual Ghostty configuration; Option+digits and Option-selection must not accidentally open it.
- [ ] Browse with arrows, then Esc. Verify the original session, cursor, scrollback, and full-size PTY viewport are unchanged; no modal keystrokes should reach the shell.
- [ ] Press n in the initial explorer. Confirm the existing shell is preserved as tab one and a new shell is selected as tab two.
- [ ] Change directory, add a session, and check that the new shell starts in that directory. Verify positional labels, explicit renaming (including wide/combining Unicode), and renumbering after removal.
- [ ] Use N to create two or more nesting levels. Switch across branches with the explorer and with successive Option+digits.
- [ ] Select a nested child, switch away through an ancestor, and switch back. The same descendant should be selected, with its screen and scrollback intact.
- [ ] Run Neovim in one branch and a noisy command in another. Switch repeatedly; verify no mixed frames or lost input.
- [ ] Resize while Neovim is visible and while it is hidden, then select it again. It should use the available content size without requiring a keystroke.
- [ ] Open the explorer over a noisy mouse-enabled TUI. Output must not overwrite the dialog, and mouse events/paste must not reach the TUI. After dismissal, clicks must land on the correct full-viewport row.
- [ ] Select and copy text before and after opening the explorer. Check drag/release across modal transitions and verify scrolling works immediately after dismissal.
- [ ] Close a subtree containing a foreground job. Cancel once, verify every child is intact, then confirm and check that only that subtree's processes terminate.
- [ ] Exit a hidden shell and confirm its tab disappears without disturbing the selected branch.
- [ ] Close down to one child and verify the group remains in the explorer; close its last child and verify empty groups disappear.
- [ ] Create more sessions than fit in the dialog. Verify scrolling, Home/End, and access beyond ten siblings. Shrink to a tiny window and enlarge again; navigation and PTY dimensions must remain valid.

Record failures with the Ghostty version, shell/editor version, terminal dimensions, exact command/input, and whether they reproduce outside tpane. See `README.md` for the documented process-cleanup limits.
