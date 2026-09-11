## Finding

**Verified:** renderer work scales with *dirty rows × viewport columns*, not actual visual differences. Ghostty explicitly permits dirty false positives. The pinned API offers batched **field queries**, but no public bulk row/cell slice API.

**Not yet proven:** FFI itself accounts for most of the reported 454 ms. The profile measures the whole `Renderer::draw_at`, including snapshot update, iteration, ANSI construction, and buffer output.

### Source locations

- `B` = `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/libghostty-vt-0.2.1`
- `S` = sibling `libghostty-vt-sys-0.2.1`
- `G` = `target/release/build/libghostty-vt-sys-9b3d98d8a98f048e/out/ghostty-src`

`Cargo.toml:14` pins `=0.2.1`; `S/build.rs:5–6` pins Ghostty **a887df42c56f6de86c0fe6da9c4eeca37931e083**. The inspected checkout’s HEAD matches.

## Cost and dirty semantics

- `src/render.rs:44–92`: every dirty row is reconstructed completely. A normal non-tail cell incurs **eight FFI calls**: next, raw-cell, wide, style, foreground, background, selected, UTF-8 grapheme. Style equality only avoids emitting redundant SGR; it does **not** avoid those lookups.
- Iterators and output `Vec` are reused; the grapheme `String` is created afresh each draw (`:61`) and reused within that draw. Its first nonempty grapheme can require an additional capacity-query/retry (`B/src/render.rs:840–892`).
- `docs/performance/native-render-diagnosis/uncapped-profile.profile.json:3–15`: 6,639 BuildFrame calls, **453,536,292 ns inclusive elapsed wall time**, versus 37,273,761 ns Feed. Approximately **68 μs per draw call**, not 454 ms per frame. Metadata explicitly says this is not CPU time and includes profiler overhead/descheduling. `scripts/profile-instrumented.py:86–94` places the span around all of `draw_at`.
- `G/src/terminal/page.zig:1985–1996` explicitly documents **dirty false positives**.
- `G/src/terminal/render.zig:363–397`: screen change, terminal/screen dirty flags, resize, or viewport-pin movement cause a full rebuild. `:554–625` also rebuilds whole page chunks when page dirty is set. `:1026–1074` unconditionally marks rebuilt rows dirty and copies their cells; there is no equality comparison with the previous rendered row.
- Cursor movement dirties old/new positions because Ghostty’s graphical renderer splits text runs around the cursor (`G/src/terminal/Screen.zig:1333–1341`). Selection set/unset dirties the screen **even if unchanged** (`:85–88`, `:2668–2669`).
- Render-state update consumes terminal dirty flags; render-state global and row flags remain independently caller-managed (`B/src/render.rs:33–49`). Current renderer clears both correctly on its successful path.

Thus, dirty does **not** prove changed text/background pixels, especially for repeated scroll content or cursor movement. Whether the investigated workload actually generates identical frames remains the parent investigation’s evidence boundary.

## Grounded options

| Option | Available now? | Correctness/performance limitations |
|---|---|---|
| Query selection once per row | **Safe Rust API:** `RowIteration::selection`, `B/src/render.rs:641`; explicitly recommended at `:900–911` | Bounds are **inclusive** (`G/src/terminal/c/render.zig:617–620`). Keep physical column counting correct across wide tails. Removes one FFI call per rendered cell. |
| Resolve ordinary style colors in Rust | Existing `Style` and palette suffice for foreground; `G/src/terminal/c/render.zig:605–616`, `G/src/terminal/style.zig:108–125,151–194` establish semantics | Background-only erased cells store color in **raw cell content**, not necessarily `Style`; cannot simply remove `bg_color()` for every cell. Foreground API does not apply bold-is-bright. Preserve that behavior. |
| Plain/unmanaged-row fast path | Row metadata and raw cells are exposed through safe getters (`B/src/screen.rs:295–308,347–370`) | `is_styled=false` does **not** imply default background: color-only cells need special handling. Row styled flag can have false positives. `codepoint()`/`wide()` still cross FFI; do not assume getter substitution alone accelerates it. |
| Batch cell fields | **C/sys API:** `ghostty_render_state_row_cells_get_multi`, `S/src/bindings.rs:3058`; corresponding row/state and raw-cell batch APIs exist | This batches fields of **one cell**, not many cells. Implementation loops through existing getters (`G/src/terminal/c/render.zig:551–571`). Stops at first non-success; absent fg/bg returns `INVALID_VALUE`, so careless batching leaves later outputs unwritten. Safe wrapper has private handles/no batch methods. Requires binding work or owning sys handles, not layout transmutation. |
| Genuine bulk snapshot export / native encoding | Internal Zig already has contiguous raw/style/grapheme arrays (`G/src/terminal/c/render.zig:25–50,759–772`) | **Not exposed publicly.** Requires upstream/custom bridge work. Directly decoding opaque C handle layouts is unsupported. Bulk export must preserve conditional style/grapheme validity and snapshot lifetime. |
| Compare/cache previously rendered rows | Feasible renderer-level strategy | Comparing fully encoded rows saves output but **not** the costly construction/getters. Earlier comparison requires semantically complete keys; raw-cell equality alone is unsafe generally. |

## Cache correctness requirements

- **Colors:** include palette, optional terminal defaults, reverse mode, and selection inversion. Snapshot default foreground/background are already reversed (`G/src/terminal/render.zig:153–160,426–445`); substituting them into today’s separate reverse-XOR path risks double inversion. Underline palette colors also matter.
- **Styles/graphemes:** styles are page-local and can be released/remapped (`G/src/terminal/Screen.zig:1323–1386`). A raw style ID is not a persistent global style key. Raw grapheme cells contain the base codepoint, while extra codepoints live separately (`G/src/terminal/render.zig:249–263,1151–1167`). Equal raw cells need not mean equal appearance.
- **Cursor:** compare/emit position, visibility, shape, and blinking independently of cached row text. Snapshot refresh always updates cursor fields (`G/src/terminal/render.zig:399–414`). Cursor-style-only stream handling does not itself mark rows dirty (`G/src/terminal/stream_terminal.zig:184–202`), so avoid assuming dirty rows fully describe cursor changes.
- **Wide text:** preserve tail skipping and spacer-head blanking (`src/render.rs:68–84`). Partial-cell damage must account for both old and new wide-cell footprints.
- **Resize/tab switching:** cache validity must include dimensions, pane identity, placement, and host-screen ownership. `src/app.rs:349–380` can clear the host and force redraw despite unchanged pane contents. Never suppress forced redraw merely because a pane-local cache matches.
- Cached row/grapheme pointers cannot survive render-state updates: rebuilt-row arenas are reset (`G/src/terminal/render.zig:1037–1044`).

## Formatter is not a drop-in renderer

`B/src/fmt.rs` exposes plain/VT/HTML formatter APIs, but the formatter serializes terminal contents, not dirty render-state rows:

- Default bounds cover **the whole screen including scrollback**, not just viewport (`G/src/terminal/formatter.zig:733–739`); a selection can restrict bounds.
- Selection means **content restriction**, not selection highlighting.
- VT emits CRLF and skips wide spacers (`:1052–1068,1121–1125`); existing renderer instead uses absolute row positioning with host autowrap disabled.
- Optional palette/modes/tabstops mutate host-global state (`:257–263,302–318,363–377`); cursor CUP has no pane offset (`:655–660`).
- Buffer-too-small handling reruns formatting to measure required size (`G/src/terminal/c/formatter.zig:176–186`).

It could inform a dedicated native row encoder, but replacing `draw_at` with formatter output would change substantial rendering semantics.

**No edits or CPU benchmarks performed.**