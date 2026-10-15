# Input keyboard settings

Keyboard settings use the bounded action/chord parser in
`src/config/keybindings.rs` and the input-only modal state in
`src/ui/input/vim.rs`. App startup reads trusted `[keys]` overrides and
`vim_mode`; `/keys` lists the effective bindings and `/vim [on|off]` changes
Vim editing for this UI session. The status bar shows the editing mode and
the literal query while searching. These input commands do not rebuild the
agent, warm provider lists, save transcript history or admit model work.

The parser retains the existing editor and App action names and default chord
table. An override has at most eight chords, each at most 40 bytes. Unknown
actions, invalid modifiers, duplicate explicit chords and control characters
produce static errors without echoing configuration values. Explicit chords
replace every default alias for that action and take precedence over another
action's default. An empty array unbinds an action. Explicit chords cannot
be assigned to two actions. Editor and global Ctrl+D defaults retain their
existing context-dependent behavior. Picker, approval, Question, plan and Git
confirmation choices retain their raw operator keys; configuration never
synthesizes an approval. Keyboard preferences require project config trust.

Vim starts in Insert mode when enabled. Escape enters Normal mode; `hjkl`,
`w`, `b`, `0`, `$`, `x`, `dd`, `u`, `i`, `a`, `I` and `A` affect only the input
draft. `/` collects a literal search of at most 256 bytes; Enter finds the next
match with wrapping, `n` repeats it, and Escape cancels. Movements preserve UTF-8
byte boundaries. Active pickers retain their existing handling.

The input editor records at most 32 undo snapshots with an aggregate limit of
2 MiB while Vim is enabled or an undo shortcut is configured. Paste uses the
existing sanitizer. Loading a draft, clearing or submitting it clears undo
history; oversized drafts clear history. Picker and operator modal boundaries
also clear undo history and keep the raw key for the event that closes them.
With Vim disabled and no undo shortcut, editing does not copy undo snapshots.

Validation follows code-indexer evidence for parser, input and paste entry
points, cursor helpers, App callers and their owning tests. The default/advisor
batch passed 22 selected tests; the final picker correction passed seven
affected tests, including the actual App command/file picker fallback and raw
Enter handoff with a replaced submit shortcut. Existing standard and session
picker event loops pass on a real 1 MiB thread stack. The installed binary also
passed an owned tmux TUI smoke at a physical 1 MiB stack: Unicode editing,
remapped shortcuts, Vim deletion/undo/search, picker boundaries, runtime Vim
control, normal quit and no completion requests. Strict Clippy and the required
debug install pass. Independent review's picker finding was corrected. See
`docs/reviews/2026-10-14-input-keybindings.json`; no local full suite was run.

## Terminal hangup and reader ownership

On Unix, terminal EOF and fatal read errors return from the local Crossterm
reader to the existing bounded input-error policy. The application still joins
its input thread when pausing or shutting down. The dependency patch and upstream
attribution are in `patched-deps/crossterm/UPSTREAM.md`.

The owned macOS regression first verifies raw mode and actual typed-input echo
before closing the PTY. It uses an isolated local metadata endpoint and sends no
model request. Against an installed debug binary:

```sh
python3 scripts/native-terminal-hangup118.py --binary /path/to/mini-agent --label normal --mode quit
python3 scripts/native-terminal-hangup118.py --binary /path/to/mini-agent --label hangup
```

The normal control checks multiline bracketed paste and `/quit`; the hangup
control requires the process to exit and be reaped without forced cleanup.
