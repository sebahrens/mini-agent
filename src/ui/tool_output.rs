//! Compact, foldable transcript rendering of tool results.
//!
//! A tool result enters the feed as a folded block: a one-line summary under
//! its call (size, or the error's first line) that unfolds to the full
//! output with Ctrl+T, `/expand`, or a click on the summary row. The
//! `tool_output` config key picks the initial form (see
//! [`crate::config::Config::resolve_tool_output`]).

use crate::config::ResolvedToolOutput;
use crate::ui::events::sanitize_output;
use crate::ui::utils::display_width;

/// Glyph of a folded result that can be unfolded.
pub(crate) const FOLDED: &str = "▸";
/// Glyph of an unfolded result that can be folded again.
pub(crate) const UNFOLDED: &str = "▾";
/// Glyph of a result with nothing more to show.
pub(crate) const PLAIN: &str = "⎿";
/// Glyph of a failed tool call.
pub(crate) const FAILED: &str = "✗";

/// Longest single-line output shown verbatim in the summary row.
const MAX_INLINE_CHARS: usize = 100;
/// Longest error first line quoted in the summary row.
const MAX_ERROR_CHARS: usize = 100;

/// The two forms a tool result can take in the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolResultView {
    /// Shown while folded.
    pub collapsed: String,
    /// Shown while unfolded.
    pub expanded: String,
    /// Whether the block starts unfolded (`tool_output = "full"`).
    pub start_expanded: bool,
}

impl ToolResultView {
    /// A result whose folded and unfolded forms differ can be toggled.
    pub(crate) fn foldable(&self) -> bool {
        self.collapsed != self.expanded
    }

    /// The form shown first.
    pub(crate) fn initial(&self) -> &str {
        if self.start_expanded {
            &self.expanded
        } else {
            &self.collapsed
        }
    }
}

/// Heuristic error detection for replayed results, which do not persist the
/// live `is_error` flag.
pub(crate) fn looks_like_error(output: &str) -> bool {
    let head: String = output
        .trim_start()
        .chars()
        .take(24)
        .collect::<String>()
        .to_ascii_lowercase();
    ["error", "tool error", "permission denied", "denied:"]
        .iter()
        .any(|prefix| head.starts_with(prefix))
}

fn plural(count: usize, one: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {one}s")
    }
}

fn size_label(lines: usize, chars: usize) -> String {
    format!("{}, {}", plural(lines, "line"), plural(chars, "char"))
}

/// Cut `text` to at most `max` display columns, marking a cut with `…`.
fn truncate_display(text: &str, max: usize) -> (String, bool) {
    if display_width(text) <= max {
        return (text.to_string(), false);
    }
    let mut out = String::new();
    let mut width = 0;
    for c in text.chars() {
        let w = display_width(c.encode_utf8(&mut [0; 4]));
        if width + w > max.saturating_sub(1) {
            break;
        }
        width += w;
        out.push(c);
    }
    out.push('…');
    (out, true)
}

/// Build the folded and unfolded transcript forms of one tool result.
pub(crate) fn tool_result_view(
    output: &str,
    is_error: bool,
    mode: ResolvedToolOutput,
) -> ToolResultView {
    let sanitized = sanitize_output(output);
    let body = sanitized.trim_end_matches('\n');
    let lines: Vec<&str> = body.lines().collect();
    let line_count = lines.len();
    let char_count = body.chars().count();

    if body.trim().is_empty() {
        let text = if is_error {
            format!("  {FAILED} error (no output)")
        } else {
            format!("  {PLAIN} (no output)")
        };
        return ToolResultView {
            collapsed: text.clone(),
            expanded: text,
            start_expanded: false,
        };
    }

    let first = lines
        .iter()
        .map(|line| line.trim())
        .find(|line| !line.is_empty())
        .unwrap_or("");

    if !is_error && line_count == 1 && char_count <= MAX_INLINE_CHARS {
        let text = format!("  {PLAIN} {}", first);
        return ToolResultView {
            collapsed: text.clone(),
            expanded: text,
            start_expanded: false,
        };
    }

    let size = size_label(line_count, char_count);
    let collapsed_header = if is_error {
        let (quoted, cut) = truncate_display(first, MAX_ERROR_CHARS);
        if line_count > 1 || cut {
            format!("  {FAILED} {quoted} ({size})")
        } else {
            format!("  {FAILED} {quoted}")
        }
    } else {
        format!("  {FOLDED} {size}")
    };
    let expanded_header = if is_error {
        format!("  {FAILED} error · {size}")
    } else {
        format!("  {UNFOLDED} {size}")
    };
    let expanded = format!("{expanded_header}\n{body}");

    let collapsed = match mode {
        ResolvedToolOutput::Preview(max_lines) if max_lines > 0 => {
            if line_count <= max_lines {
                // Everything fits in the preview: nothing left to unfold.
                let header = if is_error {
                    expanded_header.clone()
                } else {
                    format!("  {PLAIN} {size}")
                };
                let text = format!("{header}\n{body}");
                return ToolResultView {
                    collapsed: text.clone(),
                    expanded: text,
                    start_expanded: false,
                };
            }
            let hidden = line_count - max_lines;
            format!(
                "{collapsed_header}\n{}\n  … {hidden} more {}",
                lines[..max_lines].join("\n"),
                if hidden == 1 { "line" } else { "lines" }
            )
        }
        _ => collapsed_header,
    };
    ToolResultView {
        collapsed,
        expanded,
        start_expanded: mode == ResolvedToolOutput::Full,
    }
}

/// Which tool results `/expand` or `/collapse` acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FoldTarget {
    /// The `n`th most recent result (1 = latest).
    Nth(usize),
    /// Every retained result.
    All,
}

/// The fold command (`/expand` or `/collapse`) heading `text`, and its
/// argument, when `text` is one. `true` means expand.
pub(crate) fn fold_command(text: &str) -> Option<(bool, &str)> {
    let text = text.trim();
    for (command, expand) in [("/expand", true), ("/collapse", false)] {
        if let Some(rest) = text.strip_prefix(command)
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            return Some((expand, rest.trim()));
        }
    }
    None
}

/// Parse the `[n|all]` argument of `/expand` and `/collapse`.
pub(crate) fn parse_fold_target(arg: &str) -> Result<FoldTarget, String> {
    match arg.trim() {
        "" => Ok(FoldTarget::Nth(1)),
        "all" => Ok(FoldTarget::All),
        n => match n.parse::<usize>() {
            Ok(n) if n >= 1 => Ok(FoldTarget::Nth(n)),
            _ => Err(format!(
                "expected a tool result number (1 = latest) or 'all', got '{n}'"
            )),
        },
    }
}
