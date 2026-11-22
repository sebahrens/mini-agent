use crossterm::style::Color;
use unicode_width::UnicodeWidthStr;

use crate::extras::truncate::truncate_cjk;

const TOOL_SUMMARY_MAX: usize = 200;

fn display_value(val: &str) -> String {
    if val.len() <= TOOL_SUMMARY_MAX {
        format!("\"{}\"", val)
    } else {
        format!("\"{}\"", truncate_cjk(val, TOOL_SUMMARY_MAX, "..."))
    }
}

/// Returns the display width of a string in terminal columns.
/// CJK characters typically occupy 2 columns; ASCII occupies 1.
#[inline]
pub(crate) fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Returns the display width of a single character.
#[inline]
pub(crate) fn char_display_width(c: char) -> usize {
    unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Returns the longest UTF-8 prefix that fits in `max_width` terminal columns.
/// Unlike `chars().take(...)`, this never lets a double-width glyph cross the
/// right edge of a terminal row.
pub(crate) fn display_prefix(s: &str, max_width: usize) -> &str {
    let mut width = 0usize;
    let mut end = 0usize;
    for (index, ch) in s.char_indices() {
        let char_width = char_display_width(ch);
        if width.saturating_add(char_width) > max_width {
            break;
        }
        width = width.saturating_add(char_width);
        end = index + ch.len_utf8();
    }
    &s[..end]
}

/// Longest UTF-8 suffix of `s` that fits in `max_width` terminal columns.
pub(crate) fn display_suffix(s: &str, max_width: usize) -> &str {
    let mut width = 0usize;
    let mut start = s.len();
    for (index, ch) in s.char_indices().rev() {
        let char_width = char_display_width(ch);
        if width.saturating_add(char_width) > max_width {
            break;
        }
        width = width.saturating_add(char_width);
        start = index;
    }
    &s[start..]
}

/// Hard-wrap `s` into rows of at most `width` terminal columns, preserving
/// every character (no whitespace collapsing), so an exact command or path
/// can be reviewed row by row. An empty string is one empty row.
pub(crate) fn wrap_to_width(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut row_width = 0usize;
    for ch in s.chars() {
        let char_width = char_display_width(ch);
        if row_width + char_width > width && !row.is_empty() {
            rows.push(std::mem::take(&mut row));
            row_width = 0;
        }
        row.push(ch);
        row_width += char_width;
    }
    rows.push(row);
    rows
}

/// Wrap a key-hint line into rows of at most `width` columns without cutting
/// a word. Options are separated by two spaces and a `(key) label` option is
/// kept whole on one row when it fits; a longer option (or bracketed cause)
/// wraps on single spaces, and only a word wider than `width` is hard-wrapped.
pub(crate) fn wrap_at_spaces(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows: Vec<String> = Vec::new();
    let mut row = String::new();
    for segment in s.split("  ").map(str::trim).filter(|seg| !seg.is_empty()) {
        let segment_width = display_width(segment);
        if !row.is_empty() && display_width(&row) + 2 + segment_width <= width {
            row.push_str("  ");
            row.push_str(segment);
            continue;
        }
        if !row.is_empty() {
            rows.push(std::mem::take(&mut row));
        }
        if segment_width <= width {
            row.push_str(segment);
            continue;
        }
        for word in segment.split(' ').filter(|word| !word.is_empty()) {
            if !row.is_empty() && display_width(&row) + 1 + display_width(word) <= width {
                row.push(' ');
                row.push_str(word);
                continue;
            }
            if !row.is_empty() {
                rows.push(std::mem::take(&mut row));
            }
            let mut pieces = wrap_to_width(word, width);
            row = pieces.pop().unwrap_or_default();
            rows.extend(pieces);
        }
    }
    rows.push(row);
    // Keep the line's own indent when the first row has room for it.
    let indent = s.len() - s.trim_start_matches(' ').len();
    if indent > 0 && display_width(&rows[0]) + indent <= width {
        rows[0].insert_str(0, &" ".repeat(indent));
    }
    rows
}

/// Shorten `s` to at most `max_width` columns by replacing its middle with
/// `…`, keeping the head and a longer tail: for a path, the file name at the
/// end is what identifies the target.
pub(crate) fn middle_elide(s: &str, max_width: usize) -> String {
    if display_width(s) <= max_width {
        return s.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let budget = max_width - 1; // the ellipsis
    let head_width = budget / 3;
    let head = display_prefix(s, head_width);
    let tail = display_suffix(&s[head.len()..], budget - display_width(head));
    format!("{head}…{tail}")
}

/// One-line form of a possibly multi-line value for transcript summaries:
/// the first non-empty line, and `(+N lines, M chars)` when more follows.
/// The first line itself is capped at `max_bytes`.
pub(crate) fn compact_multiline(value: &str, max_bytes: usize) -> String {
    let (first, rest) = compact_parts(value, max_bytes);
    first + &rest
}

/// `compact_multiline` split into the (capped) first line and the
/// `" (+N lines, M chars)"` suffix, which is empty for a single line.
fn compact_parts(value: &str, max_bytes: usize) -> (String, String) {
    let total_lines = value.lines().count();
    let first = value
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim_end();
    let shown = truncate_cjk(first, max_bytes, "...");
    let rest = if total_lines > 1 {
        format!(
            " (+{} lines, {} chars)",
            total_lines - 1,
            value.chars().count()
        )
    } else {
        String::new()
    };
    (shown, rest)
}

/// Resolves a color based on monochrome mode.
#[inline]
pub(crate) fn resolve_color(color: Color, monochrome: bool) -> Color {
    if monochrome {
        let _ = color;
        Color::Reset
    } else {
        color
    }
}

/// Converts an RGB color to the nearest ANSI 256 color index (16-255).
fn rgb_to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    let r = r as f32;
    let g = g as f32;
    let b = b as f32;
    // Check if grayscale: all channels within ~10% of each other
    let mean = (r + g + b) / 3.0;
    let spread = (r - mean).abs().max((g - mean).abs()).max((b - mean).abs());
    if spread < 15.0 {
        // 24 grayscale steps from 232-255
        let gs = ((mean / 255.0) * 23.0).round() as u8;
        return 232 + gs.min(23);
    }
    // 216-color cube: 6 levels per channel (0, 95, 135, 175, 215, 255)
    let levels: [f32; 6] = [0.0, 95.0, 135.0, 175.0, 215.0, 255.0];
    let nearest = |v: f32| -> u8 {
        let mut best = 0u8;
        let mut best_dist = f32::MAX;
        for (i, &l) in levels.iter().enumerate() {
            let dist = (l - v).abs();
            if dist < best_dist {
                best_dist = dist;
                best = i as u8;
            }
        }
        best
    };
    let ri = nearest(r);
    let gi = nearest(g);
    let bi = nearest(b);
    16 + 36 * ri + 6 * gi + bi
}

/// Converts any Color to its nearest ANSI 256-color equivalent.
pub(crate) fn to_ansi_256(color: Color) -> Color {
    match color {
        Color::Reset => Color::Reset,
        Color::Black => Color::AnsiValue(0),
        Color::Red => Color::AnsiValue(1),
        Color::Green => Color::AnsiValue(2),
        Color::Yellow => Color::AnsiValue(3),
        Color::Blue => Color::AnsiValue(4),
        Color::Magenta => Color::AnsiValue(5),
        Color::Cyan => Color::AnsiValue(6),
        Color::White => Color::AnsiValue(7),
        Color::Grey => Color::AnsiValue(7),
        Color::DarkGrey => Color::AnsiValue(8),
        Color::DarkRed => Color::AnsiValue(9),
        Color::DarkGreen => Color::AnsiValue(10),
        Color::DarkYellow => Color::AnsiValue(11),
        Color::DarkBlue => Color::AnsiValue(12),
        Color::DarkMagenta => Color::AnsiValue(13),
        Color::DarkCyan => Color::AnsiValue(14),
        Color::Rgb { r, g, b } => Color::AnsiValue(rgb_to_ansi256(r, g, b)),
        Color::AnsiValue(v) => Color::AnsiValue(v),
    }
}

/// Parses a color name or hex string into a crossterm Color.
pub(crate) fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim().to_lowercase();
    match s.as_str() {
        "reset" => Some(Color::Reset),
        "black" => Some(Color::Black),
        "dark_grey" | "darkgrey" | "dark_gray" | "darkgray" => Some(Color::DarkGrey),
        "red" => Some(Color::Red),
        "dark_red" | "darkred" => Some(Color::DarkRed),
        "green" => Some(Color::Green),
        "dark_green" | "darkgreen" => Some(Color::DarkGreen),
        "yellow" => Some(Color::Yellow),
        "dark_yellow" | "darkyellow" => Some(Color::DarkYellow),
        "blue" => Some(Color::Blue),
        "light_blue" | "lightblue" => Some(Color::Rgb {
            r: 0x5f,
            g: 0xaf,
            b: 0xff,
        }),
        "dark_blue" | "darkblue" => Some(Color::DarkBlue),
        "magenta" => Some(Color::Magenta),
        "dark_magenta" | "darkmagenta" => Some(Color::DarkMagenta),
        "cyan" => Some(Color::Cyan),
        "dark_cyan" | "darkcyan" => Some(Color::DarkCyan),
        "white" => Some(Color::White),
        "grey" | "gray" => Some(Color::Grey),
        _ => {
            if let Some(hex) = s.strip_prefix('#')
                && hex.len() == 6
                && hex.is_ascii()
                && let (Ok(r), Ok(g), Ok(b)) = (
                    u8::from_str_radix(&hex[0..2], 16),
                    u8::from_str_radix(&hex[2..4], 16),
                    u8::from_str_radix(&hex[4..6], 16),
                )
            {
                return Some(Color::Rgb { r, g, b });
            }
            None
        }
    }
}

/// Formats a tool call showing only the primary file/command parameter.
/// This exact form is persisted as the session's tool-call text; a bash
/// command is kept whole. The transcript uses
/// [`format_tool_call_display`] instead.
pub(crate) fn format_tool_call_summary(name: &str, args: &serde_json::Value) -> String {
    tool_call_summary(name, args, false)
}

/// One-line transcript form of a tool call: like
/// [`format_tool_call_summary`], but a multi-line value (a heredoc, a
/// `node -e` script, JavaScript source) shows only its first line plus
/// `(+N lines, M chars)`, and a bash command is capped like other values.
/// The complete input stays in the session and in the permission request.
pub(crate) fn format_tool_call_display(name: &str, args: &serde_json::Value) -> String {
    tool_call_summary(name, args, true)
}

fn compact_value(val: &str, quote: bool) -> String {
    let (first, rest) = compact_parts(val, TOOL_SUMMARY_MAX);
    if quote {
        format!("\"{first}\"{rest}")
    } else {
        first + &rest
    }
}

fn tool_call_summary(name: &str, args: &serde_json::Value, compact: bool) -> String {
    let show = |val: &str| {
        if compact {
            compact_value(val, true)
        } else {
            display_value(val)
        }
    };
    let obj = match args {
        serde_json::Value::Object(map) => map,
        _ => return name.to_string(),
    };

    if name == "task" {
        return format_task_summary(obj, &show);
    }

    let primary_keys: &[&str] = match name {
        "read" | "write" | "edit" | "list_dir" | "notebook_edit" | "lsp_diagnostics" => &["path"],
        "grep" => &["pattern", "path"],
        "find_files" => &["pattern"],
        "bash" | "shell" => &["command"],
        "web_fetch" => &["url"],
        "web_search" => &["query"],
        "js" => &["code"],
        "lsp_navigate" => &["op", "path"],
        "job_status" => &["action", "id"],
        _ => &[],
    };
    let is_shell = matches!(name, "bash" | "shell");

    let mut shown = Vec::new();
    for key in primary_keys {
        let val = match obj.get(*key) {
            Some(serde_json::Value::String(val)) => val.clone(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            _ => continue,
        };
        let display_val = match (is_shell, compact) {
            (true, false) => val,
            (true, true) => compact_value(&val, false),
            _ => show(&val),
        };
        shown.push(display_val);
    }

    if shown.is_empty() {
        // Unknown tools (MCP servers): a bounded `key=value` list of the scalar
        // arguments, so the line does not depend on the model's key order.
        const MAX_PAIRS: usize = 4;
        let mut pairs: Vec<String> = obj
            .iter()
            .filter_map(|(key, val)| match val {
                serde_json::Value::String(s) => Some(format!("{key}={}", show(s))),
                serde_json::Value::Number(n) => Some(format!("{key}={n}")),
                serde_json::Value::Bool(b) => Some(format!("{key}={b}")),
                _ => None,
            })
            .collect();
        pairs.sort();
        pairs.truncate(MAX_PAIRS);
        if pairs.is_empty() {
            name.to_string()
        } else {
            format!("{} {}", name, pairs.join(" "))
        }
    } else {
        format!("{} {}", name, shown.join(" "))
    }
}

fn format_task_summary(
    obj: &serde_json::Map<String, serde_json::Value>,
    show: &dyn Fn(&str) -> String,
) -> String {
    // The model's own short label, else the first brief's label or objective.
    let label = obj
        .get("label")
        .and_then(|v| v.as_str())
        .or_else(|| {
            let first = obj.get("briefs")?.as_array()?.first()?;
            first
                .get("label")
                .or_else(|| first.get("objective"))?
                .as_str()
        })
        .filter(|text| !text.trim().is_empty());
    if let Some(label) = label {
        return format!("task {}", show(label));
    }
    let prompts = match obj.get("prompts") {
        Some(serde_json::Value::Array(arr)) => arr,
        _ => return "task".to_string(),
    };
    let parts: Vec<String> = prompts
        .iter()
        .filter_map(|v| v.as_str())
        .map(show)
        .collect();
    if parts.is_empty() {
        "task".to_string()
    } else {
        format!("task {}", parts.join(" "))
    }
}

/// Suggests a permission allow pattern for a tool+input combination.
pub(crate) fn suggest_pattern(tool: &str, input: &str) -> String {
    match tool {
        "bash" | "shell" => input.to_string(),
        "lsp_diagnostics" | "lsp_navigate" => {
            let expanded = crate::fs::expand_tilde(input);
            let path = std::path::Path::new(&expanded);
            // Aggregate LSP prompts carry the canonical project directory;
            // scope AllowAlways to that tree, not its parent. Explicit LSP
            // queries are regular files and retain the normal parent scope.
            let scope = if path.is_dir() {
                path
            } else {
                path.parent().unwrap_or(path)
            };
            descendant_pattern(scope)
        }
        "read" | "write" | "edit" | "list_dir" => {
            let expanded = crate::fs::expand_tilde(input);
            let path = std::path::Path::new(&expanded);
            let parent = path.parent().unwrap_or(path);
            descendant_pattern(parent)
        }
        // The tool side supplies the matching exact-root scope as an
        // additional pattern (see `search_root_allow_scope`); this fallback
        // is the literal tree, never a `first_token*` prefix glob.
        "grep" | "find_files" => crate::permission::pattern::search_root_allow_scope(input).0,
        // Permission inputs for non-path tools are already canonical keys
        // (for example `mcp_tool:{server}:{tool}` or `git:commit`).  Grant
        // exactly that operation; a generated wildcard here would silently
        // widen one approval to every operation in the tool family. The
        // remaining path tools (`js/read_file`, `js/write_file`) grant exactly
        // the prompted path, never a glob a `*` in that path would spell.
        _ => crate::permission::checker::exact_allow_scope_for_input(tool, input)
            .unwrap_or_else(|| input.to_string()),
    }
}

fn descendant_pattern(path: &std::path::Path) -> String {
    crate::permission::pattern::descendant_path_pattern(path)
}

#[cfg(test)]
mod tests {
    // mini-agent-ap45a: a task call shows its label, then a brief's objective.
    #[test]
    fn task_calls_show_their_label_or_first_brief() {
        let shown = |args: serde_json::Value| format_tool_call_display("task", &args);
        assert_eq!(
            shown(serde_json::json!({"label": "Map auth", "prompts": ["long question"]})),
            "task \"Map auth\""
        );
        assert_eq!(
            shown(serde_json::json!({"briefs": [{"objective": "audit auth", "label": "Audit"}]})),
            "task \"Audit\""
        );
        assert_eq!(
            shown(serde_json::json!({"briefs": [{"objective": "audit auth"}]})),
            "task \"audit auth\""
        );
        assert_eq!(shown(serde_json::json!({"prompts": ["q"]})), "task q");
    }

    #[test]
    fn parse_color_rejects_multibyte_hex_without_panicking() {
        // mini-agent-f4hxq: 6 bytes, but not on char boundaries.
        assert_eq!(super::parse_color("#a€bc"), None);
        assert_eq!(super::parse_color("#ab€1"), None);
        assert_eq!(
            super::parse_color("#00ff80"),
            Some(crossterm::style::Color::Rgb {
                r: 0,
                g: 255,
                b: 128
            })
        );
    }

    use super::{
        compact_multiline, display_prefix, display_suffix, display_width, format_tool_call_display,
        format_tool_call_summary, middle_elide, suggest_pattern, wrap_to_width,
    };

    #[test]
    fn wrap_to_width_keeps_every_character_within_the_width() {
        assert_eq!(wrap_to_width("abcdefg", 3), ["abc", "def", "g"]);
        assert_eq!(wrap_to_width("a  b", 2), ["a ", " b"]);
        assert_eq!(wrap_to_width("", 5), [""]);
        let rows = wrap_to_width("界界界", 3);
        assert_eq!(rows, ["界", "界", "界"]);
        assert!(rows.iter().all(|row| display_width(row) <= 3));
    }

    #[test]
    fn middle_elide_keeps_the_file_name_tail() {
        let path = "/home/user/projects/very/deeply/nested/directory/structure/main.rs";
        let elided = middle_elide(path, 30);
        assert!(display_width(&elided) <= 30, "{elided}");
        assert!(elided.starts_with("/home"), "{elided}");
        assert!(elided.ends_with("structure/main.rs"), "{elided}");
        assert!(elided.contains('…'));
        assert_eq!(middle_elide("short", 30), "short");
        assert_eq!(display_suffix("ab界", 2), "界");
    }

    #[test]
    fn compact_multiline_shows_the_first_line_and_a_count() {
        assert_eq!(compact_multiline("ls -la", 200), "ls -la");
        assert_eq!(
            compact_multiline("cat <<'EOF' > f\nline\nEOF", 200),
            "cat <<'EOF' > f (+2 lines, 24 chars)"
        );
        assert_eq!(
            compact_multiline("\n\nnode -e '1'\nx", 200),
            "node -e '1' (+3 lines, 15 chars)"
        );
    }

    #[test]
    fn transcript_tool_lines_compact_scripts_but_the_session_keeps_them() {
        let script = "node -e '\nconst a = 1;\nconsole.log(a);\n'";
        let args = serde_json::json!({ "command": script });
        assert_eq!(
            format_tool_call_summary("bash", &args),
            format!("bash {script}")
        );
        assert_eq!(
            format_tool_call_display("bash", &args),
            "bash node -e ' (+3 lines, 40 chars)"
        );
        let long = "x".repeat(500);
        let long_args = serde_json::json!({ "command": long });
        assert!(format_tool_call_display("bash", &long_args).len() < 220);
        let js = serde_json::json!({ "code": "const x = 1;\nreturn x;" });
        assert_eq!(
            format_tool_call_display("js", &js),
            "js \"const x = 1;\" (+1 lines, 22 chars)"
        );
        let shell = serde_json::json!({ "background": true, "command": "npm run dev" });
        assert_eq!(
            format_tool_call_summary("shell", &shell),
            "shell npm run dev"
        );
        let fetch =
            serde_json::json!({ "prompt": "find the changelog", "url": "https://x.example" });
        assert_eq!(
            format_tool_call_display("web_fetch", &fetch),
            "web_fetch \"https://x.example\""
        );
        let nav = serde_json::json!({ "op": "references", "path": "a.rs" });
        assert_eq!(
            format_tool_call_display("lsp_navigate", &nav),
            "lsp_navigate \"references\" \"a.rs\""
        );
        let mcp = serde_json::json!({ "n": 3, "flag": true, "obj": {}, "q": "hi" });
        assert_eq!(
            format_tool_call_display("srv__tool", &mcp),
            "srv__tool flag=true n=3 q=\"hi\""
        );
        let read = serde_json::json!({ "path": "/a/b.rs" });
        assert_eq!(
            format_tool_call_display("read", &read),
            format_tool_call_summary("read", &read)
        );
    }

    #[test]
    fn display_prefix_respects_terminal_columns_and_utf8_boundaries() {
        assert_eq!(display_prefix("ab界cd", 3), "ab");
        assert_eq!(display_prefix("ab界cd", 4), "ab界");
        assert!(display_width(display_prefix("界界", 3)) <= 3);
    }

    #[test]
    fn bash_suggestion_is_the_exact_complete_script() {
        let script = "echo  hello\nprintf 'done\\n'";

        assert_eq!(suggest_pattern("bash", script), script);
        assert_eq!(suggest_pattern("shell", script), script);
    }

    #[test]
    fn non_path_suggestions_are_exact_permission_keys() {
        assert_eq!(
            suggest_pattern("mcp_tool", "mcp_tool:context7:search_docs"),
            "mcp_tool:context7:search_docs"
        );
        assert_eq!(suggest_pattern("git", "git:commit"), "git:commit");
        assert_eq!(suggest_pattern("task", "task:review"), "task:review");
    }

    #[test]
    fn js_path_suggestions_keep_glob_characters_literal() {
        let pattern = suggest_pattern("js/write_file", "/workspace/**");
        assert_eq!(
            crate::permission::pattern::describe_allow_pattern(&pattern),
            "exactly /workspace/**"
        );
        let matcher =
            crate::permission::pattern::Pattern::new_generated_path_scope(&pattern).unwrap();
        assert!(matcher.matches_path("/workspace/**"));
        assert!(!matcher.matches_path("/workspace/sibling.txt"));
    }

    #[test]
    fn lsp_suggestion_uses_canonical_path_tool_scope() {
        let path = if cfg!(windows) {
            r"C:\workspace\src\main.rs"
        } else {
            "/workspace/src/main.rs"
        };
        let pattern = suggest_pattern("lsp_diagnostics", path);
        assert_ne!(pattern, "*");
        assert!(
            crate::permission::pattern::Pattern::new_generated_path_scope(&pattern).is_some(),
            "{pattern}"
        );
    }

    #[test]
    fn search_suggestion_is_the_literal_root_tree_not_a_token_prefix() {
        let root = if cfg!(windows) {
            r"C:\Users\seb\my other"
        } else {
            "/Users/seb/my other"
        };
        for tool in ["grep", "find_files"] {
            let pattern = suggest_pattern(tool, root);
            let matcher =
                crate::permission::pattern::Pattern::new_generated_path_scope(&pattern).unwrap();
            let child = std::path::Path::new(root).join("sub").join("f.rs");
            assert!(matcher.matches_path(child.to_str().unwrap()), "{tool}");
            let prefix_sibling = format!("{root}-secrets");
            assert!(!matcher.matches_path(&prefix_sibling), "{tool}");
            let cut_at_space = root.split_whitespace().next().unwrap();
            assert!(!matcher.matches_path(&format!("{cut_at_space}x")), "{tool}");
        }
    }

    #[test]
    fn lsp_project_suggestion_does_not_grant_sibling_tree() {
        let parent = std::env::temp_dir().join(format!(
            "mini-agent-lsp-pattern-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let project = parent.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let pattern = suggest_pattern("lsp_diagnostics", project.to_str().unwrap());
        let matcher =
            crate::permission::pattern::Pattern::new_generated_path_scope(&pattern).unwrap();
        assert!(matcher.matches_path(project.join("src/main.rs").to_str().unwrap()));
        assert!(!matcher.matches_path(parent.join("sibling/secret.rs").to_str().unwrap()));
        let _ = std::fs::remove_dir_all(parent);
    }
}
