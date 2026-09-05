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

pub const MAX_SESSION_IMPORT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_SESSION_IMPORT_MESSAGES: usize = 10_000;
const MAX_SESSION_IMPORT_LINE_BYTES: usize = 4 * 1024 * 1024;

/// Export a session as JSONL: one metadata header line, then one message per
/// line. Refuses output outside the same bounds [`parse_session_file`] accepts.
pub fn session_to_jsonl(session: &Session) -> Result<String> {
    if session.messages.len() > MAX_SESSION_IMPORT_MESSAGES {
        anyhow::bail!(
            "session contains more than {} messages",
            MAX_SESSION_IMPORT_MESSAGES
        );
    }
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
    let header = header.to_string();
    if header.len() > MAX_SESSION_IMPORT_LINE_BYTES {
        anyhow::bail!("session header exceeds the JSONL line limit");
    }
    out.push_str(&header);
    for msg in &session.messages {
        let mut line = serde_json::json!({
            "role": msg.role,
            "content": msg.content.as_str(),
            "estimated_tokens": msg.estimated_tokens,
        });
        if let Some(id) = &msg.tool_call_id {
            line["tool_call_id"] = serde_json::Value::String(id.to_string());
        }
        if let Some(tool) = &msg.tool {
            line["tool"] =
                serde_json::to_value(tool).context("serialize structured tool record")?;
        }
        let line = line.to_string();
        if line.len() > MAX_SESSION_IMPORT_LINE_BYTES {
            anyhow::bail!("session message exceeds the JSONL line limit");
        }
        if out.len().saturating_add(line.len()).saturating_add(2) > MAX_SESSION_IMPORT_BYTES {
            anyhow::bail!(
                "session export exceeds the {} byte import limit",
                MAX_SESSION_IMPORT_BYTES
            );
        }
        out.push('\n');
        out.push_str(&line);
    }
    out.push('\n');
    Ok(out)
}

/// Tolerant import shape: `estimated_tokens` is optional so JSONL produced by
/// other tools (bare `{role, content}` lines) also imports.
#[derive(Deserialize)]
struct ImportMessage {
    role: MessageRole,
    content: CompactString,
    #[serde(default)]
    estimated_tokens: u64,
    #[serde(default)]
    tool_call_id: Option<CompactString>,
    #[serde(default)]
    tool: Option<crate::session::PersistedToolMessage>,
}

#[derive(Deserialize)]
struct JsonlSessionHeader {
    #[serde(rename = "type")]
    kind: CompactString,
    format: CompactString,
    version: u64,
    id: CompactString,
    #[serde(default)]
    name: CompactString,
    provider: CompactString,
    model: CompactString,
    created_at: CompactString,
}

pub struct JsonlSessionImport {
    pub id: CompactString,
    pub name: CompactString,
    pub provider: CompactString,
    pub model: CompactString,
    pub created_at: CompactString,
    pub messages: Vec<SessionMessage>,
}

#[allow(clippy::large_enum_variant)]
pub enum ParsedSessionFile {
    Native(Session),
    Jsonl(JsonlSessionImport),
}

/// Detect a native Session document versus this repository's versioned JSONL
/// export by schema. Arbitrary malformed JSON never falls through to the more
/// tolerant line parser.
pub fn parse_session_file(content: &str) -> Result<ParsedSessionFile> {
    if content.len() > MAX_SESSION_IMPORT_BYTES {
        anyhow::bail!(
            "session import exceeds the {} byte limit",
            MAX_SESSION_IMPORT_BYTES
        );
    }
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let trimmed = content.trim_start();
    if trimmed.is_empty() {
        anyhow::bail!("session import is empty");
    }

    let mut values = serde_json::Deserializer::from_str(trimmed).into_iter::<serde_json::Value>();
    let first = values
        .next()
        .transpose()
        .context("first session value is not valid JSON")?
        .context("session import is empty")?;
    let trailing = &trimmed[values.byte_offset()..];
    let object = first
        .as_object()
        .context("first session value must be a JSON object")?;
    let native_shape = ["id", "messages", "provider", "model", "context_window"]
        .iter()
        .all(|field| object.contains_key(*field));
    let jsonl_marker = object.get("type").and_then(|value| value.as_str()) == Some("session")
        || object.contains_key("format")
        || object.contains_key("version");

    if native_shape && jsonl_marker {
        anyhow::bail!("ambiguous session object mixes native and JSONL schemas");
    }
    if jsonl_marker {
        return parse_jsonl_export(content).map(ParsedSessionFile::Jsonl);
    }
    if !native_shape {
        anyhow::bail!("unrecognized session schema");
    }
    if !trailing.trim().is_empty() {
        anyhow::bail!("native session JSON has trailing values");
    }

    let session: Session = serde_json::from_value(first).context("invalid native session")?;
    if session.messages.len() > MAX_SESSION_IMPORT_MESSAGES {
        anyhow::bail!(
            "native session contains more than {} messages",
            MAX_SESSION_IMPORT_MESSAGES
        );
    }
    Ok(ParsedSessionFile::Native(session))
}

fn parse_jsonl_export(content: &str) -> Result<JsonlSessionImport> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut header = None;
    let mut messages = Vec::new();
    for (index, raw_line) in content.lines().enumerate() {
        if raw_line.len() > MAX_SESSION_IMPORT_LINE_BYTES {
            anyhow::bail!(
                "line {} exceeds the {} byte limit",
                index + 1,
                MAX_SESSION_IMPORT_LINE_BYTES
            );
        }
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line)
            .with_context(|| format!("line {} is not valid JSON", index + 1))?;
        if header.is_none() {
            let parsed: JsonlSessionHeader = serde_json::from_value(value)
                .with_context(|| format!("line {} is not a JSONL session header", index + 1))?;
            if parsed.kind != "session" || parsed.format != "zerostack-session-jsonl" {
                anyhow::bail!("unsupported JSONL session format on line {}", index + 1);
            }
            if parsed.version != 1 {
                anyhow::bail!(
                    "unsupported JSONL session version {} on line {}",
                    parsed.version,
                    index + 1
                );
            }
            header = Some(parsed);
            continue;
        }
        if value.get("type").and_then(|item| item.as_str()) == Some("session")
            || value.get("format").is_some()
        {
            anyhow::bail!("duplicate JSONL session header on line {}", index + 1);
        }
        let message: ImportMessage = serde_json::from_value(value)
            .with_context(|| format!("line {} is not a session message", index + 1))?;
        messages.push(SessionMessage {
            role: message.role,
            content: message.content,
            estimated_tokens: message.estimated_tokens,
            tool_call_id: message.tool_call_id,
            tool: message.tool,
        });
        if messages.len() > MAX_SESSION_IMPORT_MESSAGES {
            anyhow::bail!(
                "JSONL session contains more than {} messages",
                MAX_SESSION_IMPORT_MESSAGES
            );
        }
    }
    let header = header.context("JSONL session header is missing")?;
    Ok(JsonlSessionImport {
        id: header.id,
        name: header.name,
        provider: header.provider,
        model: header.model,
        created_at: header.created_at,
        messages,
    })
}

/// Parse a JSONL session export back into messages. The metadata header line
/// is skipped; malformed lines error with their line number.
#[cfg(test)]
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
            tool_call_id: msg.tool_call_id,
            tool: msg.tool,
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
        .header(reqwest::header::USER_AGENT, crate::product::PUBLIC_NAME)
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
