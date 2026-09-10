# Terminal and nested-tab validation

The automated suite runs without a real Ghostty window. The following manual checks are required before declaring the part-1 compatibility pass condition met. They are not marked as completed by implementation alone.

## Open stress-test finding

Repeated parallel PTY unit-test runs can intermittently report the old `24 80` size instead of `37 101` in `pty::tests::resize_reaches_slave`, immediately after login-shell startup. An initial PTY-write `EIO` was also observed once. These have not been traced to a verified cause; ordinary full-suite runs and the interactive resize integration tests pass, but that does not resolve the stress failures.

Separately, an isolated no-shell allocator test reproduced concurrent macOS PTY allocation failures (`Unknown error: -6`). Serializing allocation eliminated those failures in that experiment, so allocation is guarded within each process. This did **not** eliminate the intermittent shell resize-test failure and must not be presented as its fix.

Before declaring part 3 complete, capture master/slave window sizes and shell signal activity across the failing resize to distinguish an application race from shell or platform behavior. Reproduce with repeated `./scripts/cargo.sh test --bin tpane pty::tests --locked` runs; do not replace this with reruns until green.

## Build

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
- [ ] Run `yes`, resize while it runs, then interrupt it. Input and shutdown should remain responsive.

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
