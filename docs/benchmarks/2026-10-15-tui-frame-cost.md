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

## Baseline (commit 115265dd, before the render-core beads)

| test | paints | flushes | writes | bytes | bytes/paint | wall |
| --- | --- | --- | --- | --- | --- | --- |
| streaming 200 KB (4286 tokens) | 2996 viewport | 145732 | 1980534 | 11425321 | 3813 | 348 ms |
| 17 keystrokes | 0 viewport, 17 bottom | 119 (7/key) | 986 | 20462 (1203/key) | - | - |
