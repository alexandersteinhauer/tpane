# Terminal multiplexer design

## Goal

Build a minimal, reliable terminal multiplexer for daily software development. The initial target is macOS, running inside Ghostty.

Keep the scope small, but make the supported features rock-solid.

## Application lifecycle

- Run as one foreground Rust application managing the UI, pane tree, and PTYs. Shells and programs run as child processes.
- No background server or client/server protocol.
- Closing the application ends the session and terminates its managed shells and programs.
- Detach/reattach and resurrection are out of scope. Do not persist layouts or restart programs across application launches.

Each terminal needs its own PTY and appropriate session/process-group setup for shell job control and signal delivery. Putting everything in one process group is not a performance goal. Avoiding a client/server boundary simplifies coordination and removes IPC, but terminal parsing and rendering are likely more important performance factors.

## Panes and nested tabs

A pane contains either a terminal session or a tab layout whose children are panes. Tab layouts can nest recursively.

Every terminal pane starts with an empty tab bar above its terminal content. Every tab layout has its own bar.

### Creating tabs

- Each bar has its own `+` button, pinned to the right.
- Clicking `+` on a terminal's empty bar turns that pane into a tab layout:
  - The existing terminal is preserved as the first child tab.
  - A new terminal is created as the second child tab.
  - Each child terminal has its own empty bar, allowing further nesting.
- Clicking `+` on an existing tab layout adds another terminal tab to that layout.
- Tab buttons share the space to the left of `+` equally.
- Handling tab overflow or excessively narrow tabs is deferred.

### Switching and closing tabs

- Clicking a tab selects its child pane.
- Only the selected child is displayed, but terminals in unselected tabs keep running and consuming output.
- All bars along the active branch remain visible, allowing switching at any ancestor level.
- Each tab layout remembers its selected child. Newly created tabs become selected immediately.
- Tabs have close buttons. Closing a tab removes its pane and terminates any terminals in its subtree. A subtree containing a foreground job requires a second mouse click on a confirmation bar.
- Removing a sibling preserves the selected child's identity. If the selected tab closes, select the next sibling, falling back to the previous sibling.
- Preserve single-child groups and their bars. Remove empty groups recursively; closing the final terminal exits the application.
- Initial labels are stable `Shell N` for terminals and `Tabs N` for nested groups. Application-supplied titles and renaming are deferred.

There are no split panes in v1.

## Terminal behavior

Each terminal should feel like an independent terminal instance:

- Start the user's login shell in each new terminal.
- Inherit the active terminal's current working directory when available; otherwise use the application's working directory.
- Run programs inside the shell. When a foreground program exits, the shell resumes naturally; do not automatically close the tab or restart the program.
- Maintain independent terminal screen state and scrollback, including while hidden.
- Support text selection and clipboard copying per terminal.
- Support normal interactive terminal behavior, including colors, cursor movement, Unicode, alternate-screen applications, resizing, and shell job control.

Use `libghostty-vt` as the preferred terminal-emulation backend through Rust bindings, with a pinned version or revision. Its engine is proven in Ghostty, but the library API and bindings are still evolving. Validate the integration before building the tab UI around it. The application remains responsible for PTYs, input routing, rendering into the host terminal, and selection/clipboard interaction.

## Input and UI

The UI is rendered inside the host terminal. Bars and buttons are mouse-clickable terminal cells, not native GUI widgets.

- Do not introduce multiplexer keyboard shortcuts in v1.
- Forward normal keyboard input to the active terminal correctly.
- Handle clicks on tab bars in the multiplexer.
- Forward mouse input within terminal content when the running application requests mouse reporting.
- Define selection, scrolling, and clipboard interactions so they coexist with applications that capture the mouse.

## Implementation principles

- Write the application in Rust.
- The MVP uses synchronous Rust with bounded PTY reader/writer threads, not an async runtime. Reevaluate async only if later I/O needs justify it. If async is introduced, use Tokio; do not assume async is inherently faster.
- Keep ownership of the pane tree and terminal state straightforward, with explicit lifecycle management. The tree contains stable IDs; a separate map owns each terminal's state, PTY, and I/O workers.
- Resize hidden terminals too, subtracting their ancestor bars and leaf bar from the host height. Clip deeper bars only when necessary to preserve one terminal row in a very short window.
- Bound output processing across terminals and rotate polling order. Switching tabs forces a redraw even if the selected terminal's render cache is clean.
- Translate mouse coordinates into the active terminal's content area. Tab controls consume their mouse press/release so clicking the UI does not inject input into a terminal.
- Perform closed-subtree process cleanup off the UI thread and wait for outstanding cleanup before application exit.
- Keep terminal parsing and state updates synchronous, separate from I/O. The current Ghostty Rust bindings expose non-`Send`, non-`Sync` terminal objects, so keep them on one thread. If Tokio is selected, a current-thread runtime and local tasks can support this architecture.
- Ensure sustained output from one terminal does not freeze the UI or prevent other terminals from progressing.
- Prioritize correct terminal behavior, resizing, process cleanup, and predictable interaction over additional features.

## MVP implementation plan

The MVP is a foreground multiplexer with nested tabs that reliably runs the user's normal shell and editor. Build it in three steps.

### 1. Prove the terminal foundation

Start with one terminal and no tabs:

- Integrate Rust with a pinned `libghostty-vt` dependency.
- Run the user's login shell through a PTY.
- Implement keyboard input, rendering, resizing, and correct Ctrl-C and shell job control.
- Support scrollback, selection, and clipboard copying.
- Cleanly shut down managed processes when the application closes.
- Validate synchronous PTY I/O with bounded reader/writer threads. Keep I/O separate from terminal state so a later Tokio integration does not require rewriting the emulator or pane model.

Pass condition: the user's shell, editor, and a mouse-enabled TUI work correctly inside Ghostty. This step validates the riskiest dependency integration before adding layout complexity. Automated tests cover the components and PTY integration; the actual host compatibility checks are tracked in [VALIDATION.md](VALIDATION.md).

### 2. Add nested tabs

- Give every terminal an empty bar with a right-aligned `+`.
- On the first `+`, preserve the existing terminal as the first tab and open a new shell as the second.
- Add sibling terminals on subsequent clicks in that group's bar.
- Support recursive nesting, visible ancestor bars, and a remembered selected child per group.
- Give tabs equal widths and close buttons.
- Keep hidden terminals running and consuming output without freezing the UI.
- Inherit the active terminal's working directory for new shells where supported.

Pass condition: the user can create, switch between, and close nested terminal tabs while programs continue running in hidden tabs.

### 3. Make it safe for daily use

Validate the following scenarios:

- Sustained output while switching tabs, without starving input or rendering.
- Resizing with deeply nested layouts.
- Closing an entire subtree and cleaning up all its managed processes.
- Programs exiting back to their shells.
- Application shutdown without orphaned managed processes.

Pass condition: these lifecycle and responsiveness checks pass, and the multiplexer supports the user's normal development workflow reliably.

### MVP interaction defaults

- Select newly created tabs.
- Confirm closing tabs whose subtree contains running foreground jobs.
- Close a terminal when its login shell exits.
- Remove empty groups; exit when the root becomes empty.

These defaults are implemented in part 2. Single-child groups retain their bars; terminal and group labels use stable IDs as described above.

The part-1 prototype uses the mouse wheel for scrollback and left-button drag selection with copy on release. When an application captures the mouse, Alt/Option requests local selection or scrolling instead. No multiplexer keyboard shortcuts are introduced.

### MVP exclusions

No detach/reattach, persistence or resurrection, split panes, multiplexer keyboard shortcuts, tab overflow handling, configuration UI, or support for platforms other than macOS.
