# Bounded terminal syntax highlighting

Fenced Markdown previously used one foreground for every code row. The TUI now
recognizes Rust, C/C++/Java/Go, JavaScript/TypeScript, Python, JSON/JSONC, TOML,
shell, and diff/patch fence labels (including the usual short aliases). Unknown
labels and indented code retain plain code styling.

The implementation in `src/ui/highlight.rs` is a small single-pass lexical scanner,
not a complete language parser. It distinguishes common keywords, strings,
comments and numbers; it preserves multiline block comments, Rust raw strings
(up to 16 delimiter hashes), Rust strings, Python/TOML triple quotes, JavaScript
backticks and shell quotes across completed streamed lines. It does not resolve
symbols, interpolation expressions, regex literals, preprocessor semantics or
arbitrary language grammars. It does not load grammar/theme files or execute code.

We considered Syntect 5.3 with bundled syntax definitions and its pure Rust
`regex-fancy` backend. That configuration avoids native Oniguruma, but its internal
regex builder provides no application-controlled per-row time/step budget. Input
byte limits alone cannot establish a deterministic bound on regex backtracking.
Tree-sitter would provide richer syntax at the cost of multiple native grammar
packages and incremental tree lifecycle. A finite scanner keeps this feature
small, offline and predictable. No new Cargo dependencies or lockfile changes
are required. See Syntect's [features](https://docs.rs/crate/syntect/5.3.0/features)
and [regex backend](https://docs.rs/syntect/5.3.0/src/syntect/parsing/regex.rs.html).

Highlighting is admitted only for agent blocks of at most 32 KiB, 512 physical
lines, and 2 KiB per line. Each fence also limits retained spans to 4,096. Crossing
a limit keeps the original literal text and existing Markdown layout, using the
plain code foreground. Every scanner iteration advances over source bytes; bounded
lookahead examines only fixed delimiters and keywords. There is no regex engine,
recursive parse, worker, timer or detached background task. Monochrome mode skips
lexical work entirely. The ordinary Markdown parser and feed's existing retention
limits still apply independently; these caps bound the added syntax work.

The feed retains the exact lexical state at its stable fence boundary. Its
separate width-independent token cache pairs literal source lines with their exact
initial state, so resizing reuses syntax while Markdown relayout remains independent.
The cache retains at most 32 KiB of source and 512 entries per block. Theme changes,
monochrome changes and replaced/pruned source invalidate it. Finalizing a stream
keeps matching completed token snapshots. Colors use the current resolved semantic
palette (keyword/link, string/addition, number/warning, comment/system, ordinary/code)
and change foregrounds only. Background, margin, selection and plain copied text
remain owned by the existing renderer. Diff headers use the link role, additions
and deletions their existing distinct roles. Model-authored diff fences remain text;
actual file-mutation records keep their separate typed event path.

Validation compares incremental and fresh parses across multiline state, CRLF and
Unicode; proves resize cache reuse and theme/monochrome invalidation; paints the
actual viewport and selections; and covers unknown languages, invalid span ranges,
unclosed/oversized fences and 10,000 literal rows with no lexical dispatch. Debug
binary size measurements use the same aarch64 macOS toolchain and default features;
this measurement is diagnostic, not a release-size claim.

Measured with Rust 1.95.0 on aarch64-apple-darwin, default features and the same
debug-install command: baseline 111,726,496 bytes; highlighted binary
111,800,496 bytes; delta +74,000 bytes
(+0.07%). No new dependencies were
linked. These unstripped development binaries include debug/build metadata; the
figures do not predict packaged release sizes.
