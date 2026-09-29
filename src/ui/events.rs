use chrono::Datelike;
use compact_str::CompactString;

use crate::cli::Cli;
use crate::config::{Config, ResolvedShowToolDetails};
use crate::context::ContextFiles;
use crate::session::{MessageRole, Session};
use crate::ui::feed::BlockStyle;
use crate::ui::renderer::Renderer;

pub fn format_time(rfc3339: &str) -> CompactString {
    let dt = chrono::DateTime::parse_from_rfc3339(rfc3339).ok();
    let dt = match dt {
        Some(dt) => dt,
        None => return CompactString::new(rfc3339),
    };
    let local = dt.with_timezone(&chrono::Local);
    let now = chrono::Local::now();
    if local.date_naive() == now.date_naive() {
        CompactString::new(local.format("%H:%M").to_string())
    } else if local.year() == now.year() {
        CompactString::new(local.format("%b %d %H:%M").to_string())
    } else {
        CompactString::new(local.format("%Y-%m-%d %H:%M").to_string())
    }
}

pub fn render_session(
    renderer: &mut Renderer,
    session: &Session,
    cli: &Cli,
    cfg: &Config,
    context: &ContextFiles,
) -> anyhow::Result<()> {
    renderer.clear_content()?;
    let feed = renderer.feed_mut();
    if context.agents.is_some() {
        feed.push_line(BlockStyle::System, "[system] loaded AGENTS.md");
        feed.push_line(BlockStyle::Plain, "");
    }
    #[cfg(feature = "archmd")]
    if context.architecture.is_some() {
        feed.push_line(BlockStyle::System, "[system] loaded ARCHITECTURE.md");
        feed.push_line(BlockStyle::Plain, "");
    }
    if !session.compactions.is_empty() {
        feed.push_line(
            BlockStyle::System,
            format!(
                "compacted {} times (saved ~{} tokens)",
                session.compactions.len(),
                session
                    .compactions
                    .last()
                    .map(|c| c.token_savings)
                    .unwrap_or(0),
            ),
        );
        feed.push_line(BlockStyle::Plain, "");
    }
    push_session_messages(feed, &session.messages, cfg);
    if session.messages.is_empty() {
        feed.push_line(
            BlockStyle::Welcome,
            welcome_header(&cli.resolve_model(cfg), &context.workspace_root),
        );
        feed.push_line(
            BlockStyle::Welcome,
            "──────────────────────────────────────────────────",
        );
        push_runtime_availability(feed, &crate::provider::js_runtime_report());
        feed.push_line(
            BlockStyle::Welcome,
            "Ready to code; type a request or '/' for commands",
        );
        feed.push_line(BlockStyle::Welcome, "Run /welcome or /tutor to get started");
        feed.push_line(BlockStyle::Plain, "");
        feed.push_line(BlockStyle::Plain, "");
    }
    Ok(())
}

/// The first welcome line. The workspace directory name comes from the
/// filesystem, so it is sanitised like any other untrusted terminal text.
fn welcome_header(model: &str, workspace_root: &std::path::Path) -> String {
    let cwd_str = workspace_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(".");
    format!(
        "[>] {} {} | {} | {}",
        crate::product::PUBLIC_NAME,
        env!("CARGO_PKG_VERSION"),
        model,
        sanitize_output(cwd_str),
    )
}

/// Replay stored messages into the feed. A tool result is placed directly
/// under its own call (matched by tool-call id), as the live view does, so a
/// response that issued several calls before any result replays as
/// call/result pairs instead of all calls followed by all results. Results
/// without a retained call are appended in stored order.
pub(crate) fn push_session_messages(
    feed: &mut crate::ui::feed::Feed,
    messages: &[crate::session::SessionMessage],
    cfg: &Config,
) {
    for msg in messages {
        match msg.role {
            // Stored text is raw (the live view sanitised only what it
            // painted), so every role is sanitised again on replay.
            MessageRole::User => {
                for line in sanitize_output(&msg.content).lines() {
                    feed.push_line(BlockStyle::User, format!("> {}", line));
                }
            }
            MessageRole::Assistant => {
                feed.push_block(BlockStyle::Agent, sanitize_output(&msg.content).to_string());
            }
            MessageRole::System => {
                for line in sanitize_output(&msg.content).lines() {
                    feed.push_line(BlockStyle::System, format!("# {}", line));
                }
            }
            MessageRole::ToolCall => {
                feed.push_anchored_block(
                    msg.tool_call_id.as_deref().unwrap_or(""),
                    BlockStyle::Tool,
                    format!("◈ {}", replayed_tool_call(&msg.content)),
                );
            }
            MessageRole::ToolResult => {
                let text = tool_result_text(&msg.content, cfg);
                let anchor = msg.tool_call_id.as_deref().unwrap_or("");
                if feed.place_after_anchor(anchor, BlockStyle::ToolResult, text) {
                    // Sits under its call, before the call's own spacer.
                    continue;
                }
            }
            MessageRole::SubagentToolCall => {
                feed.push_line(
                    BlockStyle::Tool,
                    format!("⌥ {}", replayed_tool_call(&msg.content)),
                );
            }
        }
        feed.push_line(BlockStyle::Plain, "");
    }
}

/// Announce a JavaScript runtime / learned-skill subsystem the worker
/// containment preflight refused, so the startup banner carries the signal an
/// operator would otherwise only get from `/toggle` or `--print-config`.
///
/// Pushes nothing when both subsystems are live or were never compiled in;
/// [`crate::startup::js_runtime_banner_lines`] owns that decision and the
/// verbatim reason text.
fn push_runtime_availability(
    feed: &mut crate::ui::feed::Feed,
    report: &crate::provider::JsRuntimeReport,
) {
    for line in crate::startup::js_runtime_banner_lines(report) {
        feed.push_line(BlockStyle::Error, line);
    }
}

/// The transcript text for a stored tool result, shortened per
/// `show_tool_details` like the live view.
fn tool_result_text(content: &str, cfg: &Config) -> String {
    let output = content
        .split_once(":\n")
        .map(|(_, output)| output)
        .unwrap_or(content);
    let show_details = cfg
        .show_tool_details
        .as_ref()
        .map(|s| s.resolve())
        .unwrap_or(ResolvedShowToolDetails::Limited(3));
    match show_details {
        ResolvedShowToolDetails::Off => "◈ result hidden by show_tool_details=false".to_string(),
        ResolvedShowToolDetails::Limited(max_lines) => {
            let sanitized = sanitize_output(output);
            let char_count = sanitized.chars().count();
            let lines: Vec<&str> = sanitized.lines().collect();
            if lines.len() > max_lines {
                let shown = lines[..max_lines].join("\n");
                format!(
                    "◈ result ({} chars, {} lines, showing {}):\n{}",
                    char_count,
                    lines.len(),
                    max_lines,
                    shown
                )
            } else {
                format!("◈ result ({} chars):\n{}", char_count, sanitized)
            }
        }
        ResolvedShowToolDetails::Unlimited => {
            let sanitized = sanitize_output(output);
            let char_count = sanitized.chars().count();
            format!("◈ result ({} chars):\n{}", char_count, sanitized)
        }
    }
}

pub fn show_welcome(renderer: &mut Renderer) -> std::io::Result<()> {
    let feed = renderer.feed_mut();
    feed.push_line(
        BlockStyle::Welcome,
        "──────────────────────────────────────────",
    );
    feed.push_line(
        BlockStyle::Welcome,
        format!("  {} Quickstart", crate::product::PUBLIC_NAME),
    );
    feed.push_line(
        BlockStyle::Welcome,
        "──────────────────────────────────────────",
    );
    feed.push_line(BlockStyle::Plain, "");
    feed.push_line(BlockStyle::Tool, "  Pickers:");
    feed.push_line(
        BlockStyle::Plain,
        "    @<path>     File picker / auto-complete paths",
    );
    feed.push_line(
        BlockStyle::Plain,
        "    !<command>  Run a shell command (output stored as assistant)",
    );
    feed.push_line(
        BlockStyle::Plain,
        "    .<prompt>   Switch prompt or one-shot .<prompt> <message>",
    );
    feed.push_line(
        BlockStyle::Plain,
        "    .autoconfig Guided configuration prompt",
    );
    feed.push_line(BlockStyle::Plain, "");
    feed.push_line(BlockStyle::Tool, "  Slash Commands:");
    feed.push_line(BlockStyle::Plain, "    /model        Switch model");
    feed.push_line(
        BlockStyle::Plain,
        "    /prompt       List / activate prompts",
    );
    feed.push_line(BlockStyle::Plain, "    /mode         Change security mode");
    feed.push_line(BlockStyle::Plain, "    /clear        Clear session");
    feed.push_line(BlockStyle::Plain, "    /undo         Undo last exchange");
    feed.push_line(
        BlockStyle::Plain,
        "    /compress     Free context window space",
    );
    feed.push_line(BlockStyle::Plain, "    /help         Show all commands");
    feed.push_line(BlockStyle::Plain, "");
    feed.push_line(BlockStyle::Tool, "  Keybindings:");
    feed.push_line(BlockStyle::Plain, "    Ctrl+G     Open input in $EDITOR");
    feed.push_line(BlockStyle::Plain, "    Ctrl+O     Launch lazygit");
    feed.push_line(
        BlockStyle::Plain,
        "    /command   Command picker (Tab inserts, Enter runs)",
    );
    feed.push_line(
        BlockStyle::Plain,
        "    @query     File picker (Tab/Enter inserts)",
    );
    feed.push_line(
        BlockStyle::Plain,
        format!("  Website: {}", crate::product::REPOSITORY_URL),
    );
    feed.push_line(BlockStyle::Plain, "");
    feed.push_line(
        BlockStyle::Welcome,
        "──────────────────────────────────────────",
    );
    feed.push_line(BlockStyle::Plain, "");
    Ok(())
}

/// A stored tool-call summary replayed as one transcript row: a multi-line
/// script shows its first line and a line/char count, like the live view.
fn replayed_tool_call(content: &str) -> String {
    crate::ui::utils::compact_multiline(&sanitize_output(content), 240)
}

/// Spaces a tab expands to. A raw tab moves the cursor to the terminal's next
/// tab stop, which desynchronises the renderer's width accounting.
const TAB_SPACES: &str = "    ";

/// Whether `c` is an invisible Unicode format character that can make text
/// display differently from what it is: the bidi embedding, override and
/// isolate controls (U+202A..U+202E, U+2066..U+2069), the LRM/RLM/ALM marks
/// (U+200E, U+200F, U+061C), the line and paragraph separators
/// (U+2028, U+2029), and the zero-width space, word joiner and BOM
/// (U+200B, U+2060, U+FEFF). A right-to-left override lets a prompt-injected
/// command render reversed ("Trojan Source") in the approval prompt.
///
/// ZWNJ (U+200C) and ZWJ (U+200D) are deliberately kept: Persian, Indic
/// scripts and emoji sequences need them, and they cannot reorder text.
pub fn is_deceptive_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{061c}'
            | '\u{200b}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}'
    )
}

/// Whether [`sanitize_output`] would remove or rewrite `c`: a control
/// character (ESC, a C1 introducer, BEL, `\r`, a tab, `\n`, ...) or a
/// [deceptive format character](is_deceptive_format_char). Fast paths that
/// skip sanitising clean text must use this predicate so they never pass a
/// character the sanitiser would have stripped.
pub fn needs_terminal_sanitizing(c: char) -> bool {
    c.is_control() || is_deceptive_format_char(c)
}

/// Make untrusted text safe to paint: remove every terminal control sequence
/// and control character so model or tool output can never move the cursor,
/// retitle the window, emit hyperlinks or queries, or hide text. Bidi
/// controls and the other [deceptive format
/// characters](is_deceptive_format_char) are removed too, so text can never
/// display reordered.
///
/// Parsing follows ECMA-48: CSI sequences run to their final byte, OSC/DCS/
/// SOS/PM/APC strings run to BEL or ST (and, for display robustness, to the
/// end of the line), `ESC` + intermediates + final is consumed whole, and the
/// 8-bit C1 introducers are treated like their 7-bit forms. `\r\n` becomes
/// `\n`; a lone `\r` becomes a line break rather than silently overwriting (and
/// hiding) what preceded it; a `\r` ending the chunk is dropped because a
/// streamed CRLF may be split across chunks. Tabs expand to spaces.
pub fn sanitize_output(text: &str) -> CompactString {
    sanitize(text, false)
}

/// [`sanitize_output`] for text a person must review before approving it,
/// such as a permission prompt's command: every [deceptive format
/// character](is_deceptive_format_char) is shown as a visible `<U+XXXX>`
/// marker instead of being dropped, so the reviewer sees that the request
/// carried one while the text still displays in its logical order.
pub fn sanitize_for_review(text: &str) -> CompactString {
    sanitize(text, true)
}

fn sanitize(text: &str, mark_format_chars: bool) -> CompactString {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' => result.push('\n'),
            '\r' => match chars.peek() {
                Some('\n') | None => {}
                Some(_) => result.push('\n'),
            },
            '\t' => result.push_str(TAB_SPACES),
            '\x1b' => skip_escape(&mut chars),
            '\u{9b}' => skip_csi(&mut chars),
            '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => {
                skip_control_string(&mut chars);
            }
            // C0 controls, DEL and the remaining C1 controls.
            c if c.is_control() => {}
            c if is_deceptive_format_char(c) => {
                if mark_format_chars {
                    use std::fmt::Write as _;
                    let _ = write!(result, "<U+{:04X}>", u32::from(c));
                }
            }
            c => result.push(c),
        }
    }
    CompactString::from(result)
}

type Chars<'a> = std::iter::Peekable<std::str::Chars<'a>>;

fn in_range(c: char, lo: u8, hi: u8) -> bool {
    (u32::from(lo)..=u32::from(hi)).contains(&u32::from(c))
}

/// Consume the rest of an escape sequence whose `ESC` was just read.
fn skip_escape(chars: &mut Chars<'_>) {
    let Some(&next) = chars.peek() else {
        return;
    };
    match next {
        '[' => {
            chars.next();
            skip_csi(chars);
        }
        ']' | 'P' | 'X' | '^' | '_' => {
            chars.next();
            skip_control_string(chars);
        }
        c if in_range(c, 0x20, 0x2f) => {
            // nF: intermediates, then one final byte (e.g. `ESC ( B`).
            while chars.peek().is_some_and(|&c| in_range(c, 0x20, 0x2f)) {
                chars.next();
            }
            if chars.peek().is_some_and(|&c| in_range(c, 0x30, 0x7e)) {
                chars.next();
            }
        }
        // Two-character Fp/Fe/Fs escapes (`ESC 7`, `ESC M`, `ESC c`, ...).
        c if in_range(c, 0x30, 0x7e) => {
            chars.next();
        }
        // A lone ESC: drop it and keep the following character.
        _ => {}
    }
}

/// Consume CSI parameter and intermediate bytes through the final byte. A
/// character outside the CSI grammar ends the (malformed) sequence and is
/// processed normally.
fn skip_csi(chars: &mut Chars<'_>) {
    while let Some(&c) = chars.peek() {
        if in_range(c, 0x20, 0x3f) {
            chars.next();
        } else if in_range(c, 0x40, 0x7e) {
            chars.next();
            return;
        } else {
            return;
        }
    }
}

/// Consume an OSC/DCS/SOS/PM/APC payload through its terminator: BEL, ST
/// (`ESC \` or U+009C), or a line break, which is kept.
fn skip_control_string(chars: &mut Chars<'_>) {
    while let Some(&c) = chars.peek() {
        match c {
            '\x07' | '\u{9c}' => {
                chars.next();
                return;
            }
            '\x1b' => {
                chars.next();
                if chars.peek() == Some(&'\\') {
                    chars.next();
                }
                return;
            }
            '\n' | '\r' => return,
            _ => {
                chars.next();
            }
        }
    }
}

#[cfg(test)]
mod sanitize_tests {
    use super::{needs_terminal_sanitizing, sanitize_for_review, sanitize_output};

    #[test]
    fn csi_sequences_are_removed_through_their_final_byte() {
        assert_eq!(sanitize_output("a\x1b[31;1mred\x1b[0m b"), "ared b");
        assert_eq!(sanitize_output("\x1b[<35;10;5M\x1b[97;5u"), "");
        assert_eq!(sanitize_output("x\x1b[?1049hy"), "xy");
        assert_eq!(sanitize_output("\u{9b}2Jvisible"), "visible");
    }

    #[test]
    fn osc_strings_are_removed_whole_including_titles_and_hyperlinks() {
        assert_eq!(sanitize_output("\x1b]0;evil title\x07after"), "after");
        assert_eq!(
            sanitize_output("\x1b]8;;https://x.test/a\x1b\\link\x1b]8;;\x1b\\"),
            "link"
        );
        assert_eq!(sanitize_output("\x1b]52;c;aGk=\x07ok"), "ok");
        assert_eq!(sanitize_output("\u{9d}2;t\u{9c}ok"), "ok");
        assert_eq!(sanitize_output("\x1bPq#0;1\x1b\\ok"), "ok");
    }

    #[test]
    fn unterminated_strings_stop_at_the_line_end() {
        assert_eq!(sanitize_output("\x1b]0;title\nnext line"), "\nnext line");
    }

    #[test]
    fn short_escapes_drop_only_their_own_bytes() {
        assert_eq!(sanitize_output("a\x1b7b\x1b(Bc"), "abc");
        assert_eq!(sanitize_output("a\x1b\u{e9}"), "a\u{e9}");
        assert_eq!(sanitize_output("trailing\x1b"), "trailing");
    }

    #[test]
    fn c0_c1_and_del_controls_are_removed() {
        assert_eq!(sanitize_output("a\x07b\x08c\x7fd\u{85}e\u{9a}f"), "abcdef");
    }

    #[test]
    fn carriage_returns_and_tabs_cannot_corrupt_rows() {
        assert_eq!(sanitize_output("one\r\ntwo"), "one\ntwo");
        assert_eq!(sanitize_output("rm -rf /\rsafe"), "rm -rf /\nsafe");
        assert_eq!(sanitize_output("split\r"), "split");
        assert_eq!(sanitize_output("a\tb"), "a    b");
    }

    /// mini-agent-2o379: bidi embedding/override/isolate controls, LRM/RLM/
    /// ALM, line/paragraph separators and zero-width format characters could
    /// make displayed text differ from what it is ("Trojan Source").
    #[test]
    fn bidi_and_zero_width_format_characters_are_removed() {
        let out = sanitize_output("a\u{202E}b\u{2066}c\u{2028}d\u{200B}e");
        assert_eq!(out, "abcde");
        for c in [
            '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}',
            '\u{2068}', '\u{2069}', '\u{200E}', '\u{200F}', '\u{061C}', '\u{2028}', '\u{2029}',
            '\u{200B}', '\u{2060}', '\u{FEFF}',
        ] {
            let text = format!("x{c}y");
            assert_eq!(sanitize_output(&text), "xy", "U+{:04X}", u32::from(c));
            assert!(needs_terminal_sanitizing(c));
        }
    }

    #[test]
    fn review_sanitizing_marks_format_characters_visibly() {
        assert_eq!(
            sanitize_for_review("echo ok #\u{202E} ;x\u{2069}\x1b[2J"),
            "echo ok #<U+202E> ;x<U+2069>"
        );
        assert_eq!(sanitize_for_review("\u{61C}\u{FEFF}"), "<U+061C><U+FEFF>");
        // Idempotent: the markers are plain ASCII.
        let once = sanitize_for_review("a\u{202D}b");
        assert_eq!(sanitize_for_review(&once), once);
    }

    #[test]
    fn right_to_left_scripts_and_joiner_sequences_are_untouched() {
        use crate::ui::utils::display_width;
        for text in [
            "שלום עולם",
            "مرحبا بالعالم",
            "می\u{200C}خواهم",
            "👨\u{200D}👩\u{200D}👧\u{200D}👦 family",
            "🏳\u{FE0F}\u{200D}🌈",
        ] {
            assert_eq!(sanitize_output(text), text);
            assert_eq!(sanitize_for_review(text), text);
            assert!(!text.chars().any(needs_terminal_sanitizing), "{text}");
            assert_eq!(
                display_width(&sanitize_output(text)),
                display_width(text),
                "{text}"
            );
        }
        // Removing a format character leaves the width of what remains.
        assert_eq!(display_width(&sanitize_output("ab\u{202E}cd")), 4);
        assert_eq!(display_width(&sanitize_for_review("ab\u{202E}cd")), 12);
    }

    #[test]
    fn ordinary_text_is_untouched() {
        let text = "héllo 世界 — `code` [link](https://x.test)\n";
        assert_eq!(sanitize_output(text), text);
    }
}

#[cfg(test)]
mod tests {
    use super::push_runtime_availability;
    use crate::provider::{JsRuntimeReport, RuntimeAvailability};
    use crate::ui::feed::Feed;

    /// The startup banner must carry the containment refusal, and must not
    /// grow a line when the runtime is live.
    #[test]
    fn startup_banner_shows_the_refused_runtime_and_stays_silent_otherwise() {
        const REASON: &str = "MACOS_CONTAINMENT_UNSUPPORTED_VERSION: macOS 15";

        let mut quiet = Feed::new();
        push_runtime_availability(
            &mut quiet,
            &JsRuntimeReport {
                javascript: RuntimeAvailability::Available,
                learned_skills: RuntimeAvailability::Available,
            },
        );
        assert_eq!(
            quiet.block_count(),
            0,
            "a live runtime must add no banner line"
        );

        let mut warned = Feed::new();
        push_runtime_availability(
            &mut warned,
            &JsRuntimeReport {
                javascript: RuntimeAvailability::Unavailable {
                    reason: REASON.to_string(),
                },
                learned_skills: RuntimeAvailability::Unavailable {
                    reason: format!("requires the contained JavaScript worker: {REASON}"),
                },
            },
        );
        assert_eq!(warned.block_count(), 1);
        let line = warned.block_text(0).expect("banner line");
        assert!(
            line.contains("JavaScript runtime and learned skills unavailable"),
            "{line}"
        );
        assert!(
            line.contains(REASON),
            "the reason must reach the banner: {line}"
        );
    }
}

#[cfg(test)]
mod replay_tests {
    use super::push_session_messages;
    use crate::config::Config;
    use crate::session::Session;
    use crate::ui::feed::Feed;

    fn texts(feed: &Feed) -> Vec<String> {
        (0..feed.block_count())
            .map(|index| feed.block_text(index).unwrap().to_string())
            .collect()
    }

    /// mini-agent-mfebw: a replayed session shows each result under its own
    /// call, as the live feed does, not all calls followed by all results.
    #[test]
    fn replay_places_each_tool_result_under_its_call() {
        let mut session = Session::new("openrouter", "test-model", 128_000, "/workspace");
        let args = serde_json::json!({ "path": "x" });
        session.add_tool_call_with_id("a", "read", &args);
        session.add_tool_call_with_id("b", "grep", &args);
        session.add_tool_result_with_id("b", "grep", "result-b");
        session.add_tool_result_with_id("a", "read", "result-a");
        let mut feed = Feed::new();
        push_session_messages(&mut feed, &session.messages, &Config::default());

        let rows = texts(&feed);
        let find = |needle: &str| {
            rows.iter()
                .position(|row| row.contains(needle))
                .unwrap_or_else(|| panic!("{needle} missing from {rows:?}"))
        };
        let (call_a, call_b) = (find("◈ read"), find("◈ grep"));
        let (result_a, result_b) = (find("result-a"), find("result-b"));
        assert_eq!(result_a, call_a + 1, "{rows:?}");
        assert_eq!(result_b, call_b + 1, "{rows:?}");
        assert!(call_a < call_b);
        // One spacer per call/result pair, none between a call and its result.
        assert_eq!(rows[result_a + 1], "");
        assert_eq!(rows.len(), 6, "{rows:?}");
    }

    /// mini-agent-5bvrb: the welcome screen names the lazygit key.
    #[test]
    fn welcome_names_the_lazygit_key() {
        let mut renderer = crate::ui::renderer::Renderer::new().unwrap();
        super::show_welcome(&mut renderer).unwrap();
        let feed = renderer.feed_mut();
        let rows = texts(feed);
        assert!(
            rows.iter()
                .any(|row| row.contains("Ctrl+O") && row.contains("lazygit")),
            "{rows:?}"
        );
    }

    /// True when `text` carries a byte that could start or end a terminal
    /// control sequence.
    fn has_terminal_control(text: &str) -> bool {
        text.chars()
            .any(|c| matches!(c, '\x1b' | '\u{9b}' | '\u{9d}' | '\x07'))
    }

    /// mini-agent-q60fh: stored user, assistant and system text is replayed
    /// (on --continue, rewind, session switch, ...) without the escape
    /// sequences the live view stripped, so an OSC 52 clipboard write or a
    /// screen clear in a stored message never reaches the terminal.
    #[test]
    fn replay_strips_terminal_control_sequences_from_every_role() {
        use crate::session::MessageRole;
        let mut session = Session::new("openrouter", "test-model", 128_000, "/workspace");
        session.add_shell_interaction("!cat x", "a\x1b]52;c;SGk=\x07b");
        session.add_message(MessageRole::User, "u\u{9b}2Jv\u{9d}0;title\x07w");
        session.add_message(MessageRole::Assistant, "x\x1b[2Jy\x1b[Hz");
        session.add_message(MessageRole::System, "s\x1b]0;pwned\x07t\x1b[31mred");
        let mut feed = Feed::new();
        push_session_messages(&mut feed, &session.messages, &Config::default());

        let rows = texts(&feed);
        assert!(!rows.is_empty());
        for row in &rows {
            assert!(!has_terminal_control(row), "control byte in {row:?}");
        }
        let joined = rows.join("\n");
        for visible in ["ab", "uvw", "xyz", "st", "red"] {
            assert!(joined.contains(visible), "{visible} missing: {rows:?}");
        }
        assert!(!joined.contains("SGk="), "OSC payload leaked: {rows:?}");
        assert!(!joined.contains("pwned"), "OSC payload leaked: {rows:?}");
    }

    /// mini-agent-q60fh: the welcome line names the workspace directory,
    /// whose name is attacker-influenced; it must not carry escapes.
    #[test]
    fn welcome_header_strips_control_sequences_from_the_directory_name() {
        let header = super::welcome_header(
            "model",
            std::path::Path::new("/tmp/evil\x1b]52;c;SGk=\x07dir"),
        );
        assert!(!has_terminal_control(&header), "{header:?}");
        assert!(header.ends_with("evildir"), "{header:?}");
    }

    /// Results without an id (legacy sessions) keep their stored order.
    #[test]
    fn replay_appends_results_without_a_matching_call() {
        let mut session = Session::new("openrouter", "test-model", 128_000, "/workspace");
        session.add_tool_call("read", &serde_json::json!({}));
        session.add_tool_result("read", "legacy-result");
        let mut feed = Feed::new();
        push_session_messages(&mut feed, &session.messages, &Config::default());
        let rows = texts(&feed);
        assert_eq!(rows.len(), 4, "{rows:?}");
        assert!(rows[0].starts_with("◈ read"), "{rows:?}");
        assert_eq!(rows[1], "");
        assert!(rows[2].contains("legacy-result"), "{rows:?}");
    }
}
