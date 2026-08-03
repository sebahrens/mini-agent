//! Session export, import, and sharing.
//!
//! Two interchange formats: a standalone HTML page (human-readable, for
//! sharing) and JSONL (machine-readable, re-importable via `/import`).
//! Sharing uploads the HTML export as a secret GitHub gist.

use anyhow::{Context, Result};
use compact_str::CompactString;
use pulldown_cmark::{Event, Options, Tag, TagEnd};
use serde::Deserialize;

use crate::session::{MessageRole, Session, SessionMessage};

/// Export a session as JSONL: one metadata header line, then one message per
/// line. This is the format `parse_jsonl_import` accepts back.
pub fn session_to_jsonl(session: &Session) -> String {
    let mut out = String::new();
    let header = serde_json::json!({
        "type": "session",
        "format": "zerostack-session-jsonl",
        "version": 1,
        "id": session.id.as_str(),
        "name": session.name.as_str(),
        "provider": session.provider.as_str(),
        "model": session.model.as_str(),
        "created_at": session.created_at.as_str(),
    });
    out.push_str(&header.to_string());
    for msg in &session.messages {
        out.push('\n');
        let line = serde_json::json!({
            "role": msg.role,
            "content": msg.content.as_str(),
            "estimated_tokens": msg.estimated_tokens,
        });
        out.push_str(&line.to_string());
    }
    out.push('\n');
    out
}

/// Tolerant import shape: `estimated_tokens` is optional so JSONL produced by
/// other tools (bare `{role, content}` lines) also imports.
#[derive(Deserialize)]
struct ImportMessage {
    role: MessageRole,
    content: CompactString,
    #[serde(default)]
    estimated_tokens: u64,
}

/// Parse a JSONL session export back into messages. The metadata header line
/// is skipped; malformed lines error with their line number.
pub fn parse_jsonl_import(content: &str) -> Result<Vec<SessionMessage>> {
    let mut messages = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line)
            .with_context(|| format!("line {} is not valid JSON", idx + 1))?;
        if value.get("type").and_then(|t| t.as_str()) == Some("session") {
            continue;
        }
        let msg: ImportMessage = serde_json::from_value(value)
            .with_context(|| format!("line {} is not a session message", idx + 1))?;
        messages.push(SessionMessage {
            role: msg.role,
            content: msg.content,
            estimated_tokens: msg.estimated_tokens,
        });
    }
    if messages.is_empty() {
        anyhow::bail!("no messages found");
    }
    Ok(messages)
}

/// Page template for the HTML export. `{{title}}`, `{{meta}}`, and
/// `{{messages}}` are substituted with `str::replace` (never `format!`), so
/// the CSS braces in the template stay intact.
const TEMPLATE: &str = include_str!("../../data/session_export.html");

/// Export a session as a standalone, self-contained HTML page. Assistant
/// messages are rendered from a safe Markdown subset; all other roles are
/// escaped and shown verbatim.
pub fn session_to_html(session: &Session) -> String {
    let mut messages = String::new();
    for msg in &session.messages {
        let (class, label) = role_class_label(msg, session);
        messages.push_str(&format!(
            "<section class=\"msg {}\">\n<p class=\"role\">",
            class
        ));
        escape_html_into(&mut messages, &label);
        messages.push_str("</p>\n");
        match msg.role {
            MessageRole::Assistant => {
                messages.push_str("<div class=\"markdown\">");
                push_safe_markdown(&mut messages, &msg.content);
                messages.push_str("</div>\n");
            }
            _ => {
                messages.push_str("<pre>");
                escape_html_into(&mut messages, &msg.content);
                messages.push_str("</pre>\n");
            }
        }
        messages.push_str("</section>\n");
    }

    let title = escape_html(&session_title(session));
    let meta = escape_html(&format!(
        "{} / {} · {} · {} messages · {} in / {} out tokens · ${:.4}",
        session.provider,
        session.model,
        session.created_at,
        session.messages.len(),
        session.total_input_tokens,
        session.total_output_tokens,
        session.total_cost,
    ));
    TEMPLATE
        .replace("{{title}}", &title)
        .replace("{{meta}}", &meta)
        .replace("{{messages}}", &messages)
}

/// Render Markdown without ever passing attacker-authored HTML through to the
/// exported document. Unsafe links and images lose their wrapper but retain
/// their readable label/alt text.
fn push_safe_markdown(out: &mut String, markdown: &str) {
    let parser = pulldown_cmark::Parser::new_ext(
        markdown,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS,
    );
    let mut parser = parser.into_iter();
    let mut suppressed_ends = Vec::new();
    let safe_events = std::iter::from_fn(move || {
        loop {
            let event = parser.next()?;
            match event {
                Event::Html(raw) | Event::InlineHtml(raw) => return Some(Event::Text(raw)),
                Event::Start(Tag::Link {
                    link_type,
                    dest_url,
                    title,
                    id,
                }) => {
                    if export_url_is_safe(&dest_url, false) {
                        return Some(Event::Start(Tag::Link {
                            link_type,
                            dest_url,
                            title,
                            id,
                        }));
                    } else {
                        suppressed_ends.push(TagEnd::Link);
                    }
                }
                Event::Start(Tag::Image {
                    link_type,
                    dest_url,
                    title,
                    id,
                }) => {
                    if export_url_is_safe(&dest_url, true) {
                        return Some(Event::Start(Tag::Image {
                            link_type,
                            dest_url,
                            title,
                            id,
                        }));
                    } else {
                        suppressed_ends.push(TagEnd::Image);
                    }
                }
                Event::End(end) if suppressed_ends.last() == Some(&end) => {
                    suppressed_ends.pop();
                }
                other => return Some(other),
            }
        }
    });

    pulldown_cmark::html::push_html(out, safe_events);
}

/// Allow only navigation schemes that cannot execute document script. Images
/// are stricter than links and must use HTTPS. Relative links remain usable in
/// a locally saved export, except protocol-relative URLs.
fn export_url_is_safe(raw: &str, image: bool) -> bool {
    let mut decoded = raw.to_string();
    for _ in 0..4 {
        let next = percent_decode_once(&decoded);
        if next == decoded {
            break;
        }
        decoded = next;
    }

    let candidate = decoded.trim_matches(|character: char| {
        character.is_ascii_whitespace() || character.is_ascii_control()
    });
    if candidate.is_empty() || candidate.starts_with("//") {
        return false;
    }

    let boundary = candidate
        .char_indices()
        .find(|(_, character)| matches!(character, ':' | '/' | '?' | '#'));
    let Some((index, delimiter)) = boundary else {
        return !image && !candidate.contains('%');
    };

    if candidate[..index].contains('%') {
        return false;
    }
    if delimiter != ':' {
        return !image;
    }

    let scheme: String = candidate[..index]
        .chars()
        .filter(|character| !character.is_ascii_whitespace() && !character.is_ascii_control())
        .map(|character| character.to_ascii_lowercase())
        .collect();
    if scheme.is_empty()
        || !scheme.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
        })
    {
        return false;
    }

    if image {
        scheme == "https"
    } else {
        matches!(scheme.as_str(), "http" | "https" | "mailto")
    }
}

fn percent_decode_once(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn session_title(session: &Session) -> String {
    if session.name.is_empty() {
        format!(
            "zerostack session {}",
            &session.id[..8.min(session.id.len())]
        )
    } else {
        session.name.to_string()
    }
}

fn role_class_label(msg: &SessionMessage, session: &Session) -> (&'static str, String) {
    match msg.role {
        MessageRole::User => ("user", "you".to_string()),
        MessageRole::Assistant => ("assistant", session.model.to_string()),
        MessageRole::System => ("system", "system".to_string()),
        MessageRole::ToolCall => ("tool", "tool call".to_string()),
        MessageRole::ToolResult => ("tool", "tool result".to_string()),
        MessageRole::SubagentToolCall => ("tool", "subagent tool call".to_string()),
    }
}

fn escape_html_into(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

fn escape_html(text: &str) -> String {
    let mut out = String::new();
    escape_html_into(&mut out, text);
    out
}

/// Upload `content` as a secret gist and return its URL. Requires
/// `GITHUB_TOKEN` or `GH_TOKEN` in the environment.
pub async fn share_gist(filename: &str, content: &str, description: &str) -> Result<String> {
    let token = std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .context("set GITHUB_TOKEN or GH_TOKEN to share sessions as gists")?;
    let body = serde_json::json!({
        "description": description,
        "public": false,
        "files": { filename: { "content": content } },
    });
    let response = reqwest::Client::new()
        .post("https://api.github.com/gists")
        .header(reqwest::header::USER_AGENT, "zerostack")
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", token))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .json(&body)
        .send()
        .await
        .context("failed to reach the GitHub API")?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        anyhow::bail!("GitHub API returned {}: {}", status, text.trim());
    }
    let json: serde_json::Value = response
        .json()
        .await
        .context("invalid GitHub API response")?;
    json.get("html_url")
        .and_then(|u| u.as_str())
        .map(str::to_string)
        .context("GitHub API response did not include html_url")
}
