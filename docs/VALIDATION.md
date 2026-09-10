# Part 1 validation

The automated suite runs without a real Ghostty window. The following manual checks are required before declaring the part-1 compatibility pass condition met. They are not marked as completed by implementation alone.

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

Record failures with the Ghostty version, shell/editor version, terminal dimensions, exact command/input, and whether they reproduce outside tpane. See `README.md` for the documented process-cleanup limits.
