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
    for msg in &session.messages {
        match msg.role {
            MessageRole::User => {
                for line in msg.content.lines() {
                    feed.push_line(BlockStyle::User, format!("> {}", line));
                }
            }
            MessageRole::Assistant => {
                feed.push_block(BlockStyle::Agent, msg.content.to_string());
            }
            MessageRole::System => {
                for line in msg.content.lines() {
                    feed.push_line(BlockStyle::System, format!("# {}", line));
                }
            }
            MessageRole::ToolCall => {
                feed.push_line(
                    BlockStyle::Tool,
                    format!("◈ {}", replayed_tool_call(&msg.content)),
                );
            }
            MessageRole::ToolResult => {
                render_tool_result_to_feed(feed, &msg.content, cfg)?;
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
    if session.messages.is_empty() {
        let cwd = &context.workspace_root;
        let cwd_str = cwd.file_name().and_then(|n| n.to_str()).unwrap_or(".");
        feed.push_line(
            BlockStyle::Welcome,
            format!(
                "[>] {} {} | {} | {}",
                crate::product::PUBLIC_NAME,
                env!("CARGO_PKG_VERSION"),
                cli.resolve_model(cfg),
                cwd_str,
            ),
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

fn render_tool_result_to_feed(
    feed: &mut crate::ui::feed::Feed,
    content: &str,
    cfg: &Config,
) -> anyhow::Result<()> {
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
        ResolvedShowToolDetails::Off => {
            feed.push_line(
                BlockStyle::ToolResult,
                "◈ result hidden by show_tool_details=false",
            );
        }
        ResolvedShowToolDetails::Limited(max_lines) => {
            let sanitized = sanitize_output(output);
            let char_count = sanitized.chars().count();
            let lines: Vec<&str> = sanitized.lines().collect();
            if lines.len() > max_lines {
                let shown = lines[..max_lines].join("\n");
                feed.push_line(
                    BlockStyle::ToolResult,
                    format!(
                        "◈ result ({} chars, {} lines, showing {}):\n{}",
                        char_count,
                        lines.len(),
                        max_lines,
                        shown
                    ),
                );
            } else {
                feed.push_line(
                    BlockStyle::ToolResult,
                    format!("◈ result ({} chars):\n{}", char_count, sanitized),
                );
            }
        }
        ResolvedShowToolDetails::Unlimited => {
            let sanitized = sanitize_output(output);
            let char_count = sanitized.chars().count();
            feed.push_line(
                BlockStyle::ToolResult,
                format!("◈ result ({} chars):\n{}", char_count, sanitized),
            );
        }
    }
    Ok(())
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

/// Make untrusted text safe to paint: remove every terminal control sequence
/// and control character so model or tool output can never move the cursor,
/// retitle the window, emit hyperlinks or queries, or hide text.
///
/// Parsing follows ECMA-48: CSI sequences run to their final byte, OSC/DCS/
/// SOS/PM/APC strings run to BEL or ST (and, for display robustness, to the
/// end of the line), `ESC` + intermediates + final is consumed whole, and the
/// 8-bit C1 introducers are treated like their 7-bit forms. `\r\n` becomes
/// `\n`; a lone `\r` becomes a line break rather than silently overwriting (and
/// hiding) what preceded it; a `\r` ending the chunk is dropped because a
/// streamed CRLF may be split across chunks. Tabs expand to spaces.
pub fn sanitize_output(text: &str) -> CompactString {
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
    use super::sanitize_output;

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
