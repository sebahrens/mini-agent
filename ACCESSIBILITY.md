# Accessibility

mini-agent has two switches for users of screen readers, braille displays and anyone sensitive to
motion.

| Setting | Effect |
|---|---|
| `--screen-reader` or `screen_reader = true` | Monochrome output (like `--no-color`), no mouse capture (the terminal keeps native selection), no terminal-title updates, and no spinner animation. |
| `reduce_motion = true` | Only the spinner animation: the prompt shows a static `* ` while a run is active instead of a changing braille glyph. Implied by `screen_reader`. |
| `NO_COLOR` / `--no-color` | Monochrome output. Honoured independently of the two settings above. |

Not yet delivered (tracked in the CHANGELOG as partial): a fully linear, append-only transcript on the
main screen (no alternate screen or status-bar repaint), numbered-list pickers, and +/- diff markers
that do not depend on colour. Until then, headless mode (`mini-agent -p`) produces plain linear text
and is the most predictable surface for a screen reader.
