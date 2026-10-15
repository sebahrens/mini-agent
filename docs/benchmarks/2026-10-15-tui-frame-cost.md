# TUI frame cost (2026-10-15)

Measured by the renderer's test-only output counters (`Renderer::frame_stats`,
`src/ui/renderer.rs`) in two captured-output tests:

```bash
cargo test --bin mini-agent -- frame_cost --nocapture
```

- `frame_cost_streaming_200kb_markdown` streams a 200 KB markdown response
  (headings, prose with URLs, fenced Rust code) in 48-byte tokens into a
  running agent block, repainting the way the streaming arm does
  (`stream_repaint_due`) plus a UI tick (`paint_pending_chat` +
  `tick_spinner`) every tenth token. Geometry is the captured fixture's
  180x50.
- `frame_cost_keystrokes_under_full_viewport` types 17 characters into the
  input under a full chat viewport, one `refresh_display`-shaped frame
  (`sync_input_height`, `render_viewport`, `draw_bottom`) per key.

`writes` counts `Write::write` calls reaching the output, `flushes` counts
flushes (each one is a `write(2)` on the real, line-buffered stdout), and
`bytes` counts escape plus text bytes. Wall time is a debug build on an Apple
Silicon laptop and is indicative only.

At runtime the same frames can be traced with `RUST_LOG=ui::frame=trace`
(target `ui::frame`, fields `region`, `bytes`, `elapsed_us`).

## Results

Baseline is commit 115265dd, before the render-core beads. "Rows" counts
chat rows actually repainted.

| run | paints | rows | flushes | writes | bytes | bytes/paint | wall |
| --- | --- | --- | --- | --- | --- | --- | --- |
| baseline: streaming 200 KB (4286 tokens) | 2996 viewport | 137816 | 145732 | 1980534 | 11425321 | 3813 | 348 ms |
| baseline: 17 keystrokes | 0 viewport, 17 bottom | 0 | 119 (7/key) | 986 | 20462 (1203/key) | - | - |
| cttyn.2 frame buffer: streaming | 2996 viewport | 137816 | 3425 | 3425 | 11480121 | 3831 | 280 ms |
| cttyn.2 frame buffer: 17 keystrokes | 0 viewport, 17 bottom | 0 | 17 (1/key) | 17 | 20734 (1219/key) | - | - |
| cttyn.3 row diffing: streaming | 2996 viewport | 14338 | 3424 | 3424 | 1023906 | 341 | 158 ms |
| cttyn.3 row diffing: 17 keystrokes | 0 viewport, 17 bottom | 0 | 17 (1/key) | 17 | 20734 (1219/key) | - | - |

A keystroke's bytes are the bottom region (input rows, separators and
statusline rows, the latter copied from the fitted-row cache since
cttyn.13); the chat is not repainted.

## Render pipeline

- **Frames** (cttyn.2). Every paint path writes into the renderer's frame
  buffer. `Renderer::frame` (or `begin_frame`/`end_frame`) nests; the
  outermost close wraps the bytes in a DEC 2026 synchronized update and
  writes them with one `write_all` + flush. `refresh_display` is one frame;
  a picker overlay is drawn as its own frame after it. A failed write marks
  every region for a full repaint.
- **Geometry** (cttyn.4). `terminal_size` returns geometry cached by the
  last bottom draw's native query; `resize`, terminal resume and a size that
  changed under a bottom draw refresh it.
- **Chat rows** (cttyn.3). `render_viewport` keys each screen row by its
  content, colors, syntax spans, selection and (top row) history indicator,
  and paints only rows whose key changed. Content that moved up or down is
  shifted with a scroll region confined to the chat rows. Geometry, margin,
  background or monochrome changes, `invalidate`, `resize`, `clear_content`,
  a failed frame and a terminal resume (`terminal::resume_epoch`) force a
  full repaint; an open picker marks only the `PICKER_OVERLAY_MAX_ROWS`
  band above its floor (cttyn.26).
- **Lines** (cttyn.6). `write_line` paints at once outside a frame, so text
  printed before an await stays visible; inside a frame it paints once at
  the frame's close. Synchronous slash listings, `/context`, submission
  echoes (`app::echo_submission`), `!` command echo and output, `/btw`
  answers, `/queue`, `/skills`, worktree proposals and merge-conflict
  listings, and the diagnostics/MCP-notice drains each write one frame; a
  frame always closes before the code awaits the user or a turn, so prompts
  are on screen while they wait.
- **Resume tail** (cttyn.9). Following the output without a selection,
  `render_viewport` paints from `Feed::lines_tail(width, visible)`, which
  lays out only the last blocks the viewport needs. While that layout does
  not reach the top, the history marker reads ` ↑ earlier ` instead of a
  row count; the event loop then calls `Renderer::advance_feed_layout` in
  16-block idle steps until the layout is complete and repaints the exact
  count. Scrolling, selection, clicks, `buffer_len` and a resize while
  scrolled back use the complete `Feed::lines(width)`.
- **Caches**: plain rows skip the URL regex (cttyn.11); fitted statusline
  rows are reused by key (cttyn.13); the input wrap layout is memoized per
  draft and width in the renderer, and `InputEditor` keeps its own memo for
  Up/Down row movement, validated by comparing the buffer (cttyn.19).
