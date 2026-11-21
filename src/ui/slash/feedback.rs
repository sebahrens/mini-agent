//! `/feedback` (a prefilled, redacted issue draft) and `/release-notes`.
//!
//! `/feedback [text]` shows the draft and the prefilled new-issue address;
//! `/feedback --open [text]` opens that address in the browser (or prints it
//! when none can be opened). Nothing is ever submitted: the user finishes the
//! issue on GitHub. The body carries only non-secret build and runtime facts;
//! home-directory paths and anything that looks like a credential is redacted
//! before the draft is shown, and no transcript is included unless the user
//! types it into the text.
use super::SlashCtx;

/// The CHANGELOG section for the running version, extracted by `build.rs`.
const RELEASE_NOTES: &str = include_str!(concat!(env!("OUT_DIR"), "/release_notes.md"));

/// Longest issue body placed in the address; browsers and GitHub refuse very
/// long query strings.
const MAX_BODY_BYTES: usize = 6000;

/// Replace the home directory, user-name path segments and credential-shaped
/// text. Conservative: it prefers over-redaction.
pub(crate) fn redact(text: &str, home: Option<&str>) -> String {
    let mut out = text.to_owned();
    if let Some(home) = home.filter(|h| h.len() > 1) {
        let trimmed = home.trim_end_matches(['/', '\\']);
        out = out.replace(trimmed, "~");
        out = out.replace(&trimmed.replace('\\', "/"), "~");
    }
    out.lines().map(redact_line).collect::<Vec<_>>().join("\n")
}

fn redact_line(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let sensitive_key = [
        "key",
        "token",
        "secret",
        "password",
        "passwd",
        "authorization",
        "credential",
    ]
    .iter()
    .any(|word| lower.contains(word));
    if sensitive_key && let Some(split) = line.find([':', '=']) {
        return format!("{} [redacted]", redact_words(&line[..=split]));
    }
    redact_words(line)
}

fn redact_words(line: &str) -> String {
    line.split(' ')
        .map(redact_word)
        .collect::<Vec<_>>()
        .join(" ")
}

fn redact_word(word: &str) -> String {
    let core = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
    let prefixed = [
        "sk-",
        "ghp_",
        "gho_",
        "ghs_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "AKIA",
        "AIza",
        "eyJ",
    ]
    .iter()
    .any(|prefix| core.starts_with(prefix) && core.len() >= prefix.len() + 8);
    let long_blob = core.len() >= 32
        && core
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '/' | '='))
        && core.chars().any(|c| c.is_ascii_digit())
        && core.chars().any(|c| c.is_ascii_alphabetic());
    if prefixed || long_blob {
        word.replace(core, "[redacted]")
    } else {
        // `/Users/alice/...`, `/home/alice/...`, `C:\Users\alice\...`
        for marker in ["/Users/", "/home/", "\\Users\\"] {
            if let Some(at) = word.find(marker) {
                let after = at + marker.len();
                let rest = &word[after..];
                let end = rest.find(['/', '\\']).unwrap_or(rest.len());
                return format!("{}~{}", &word[..at], &rest[end..]);
            }
        }
        word.to_owned()
    }
}

/// Non-secret facts about this build and terminal.
pub(crate) fn environment_facts() -> String {
    let term = |name: &str| std::env::var(name).unwrap_or_else(|_| "unknown".into());
    format!(
        "- version: {}\n- build: {}\n- target: {}\n- os: {}\n- terminal: {} ({})\n- features: {}",
        env!("CARGO_PKG_VERSION"),
        env!("MINI_AGENT_BUILD_FINGERPRINT"),
        env!("MINI_AGENT_BUILD_TARGET"),
        std::env::consts::OS,
        term("TERM"),
        term("TERM_PROGRAM"),
        crate::session::written_by_current()
            .split_once('(')
            .map(|(_, rest)| rest.trim_end_matches(')'))
            .unwrap_or("default"),
    )
}

/// The issue body: what happened, the facts and the redacted diagnostics.
pub(crate) fn issue_body(user_text: &str, diagnostics: &str, home: Option<&str>) -> String {
    let mut body = String::new();
    body.push_str("## What happened\n\n");
    let text = redact(user_text.trim(), home);
    body.push_str(if text.is_empty() {
        "(describe the problem here)"
    } else {
        &text
    });
    body.push_str("\n\n## Environment\n\n");
    body.push_str(&environment_facts());
    body.push_str("\n\n## Diagnostics (redacted)\n\n```\n");
    body.push_str(redact(diagnostics, home).trim());
    body.push_str("\n```\n");
    if body.len() > MAX_BODY_BYTES {
        let mut end = MAX_BODY_BYTES;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
        body.push_str("\n[truncated]\n```\n");
    }
    body
}

fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The repository's new-issue address with the title and body prefilled.
pub(crate) fn issue_url(title: &str, body: &str) -> String {
    format!(
        "{}/issues/new?title={}&body={}",
        crate::product::REPOSITORY_URL,
        percent_encode(title),
        percent_encode(body)
    )
}

/// The embedded CHANGELOG section for the running version.
pub(crate) fn release_notes() -> &'static str {
    RELEASE_NOTES
}

pub(crate) async fn handle_feedback(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let open = parts.get(1) == Some(&"--open");
    let text = parts[if open { 2 } else { 1 }.min(parts.len())..].join(" ");
    let diagnostics = match crate::paths::process_paths_ref() {
        Ok(paths) => crate::diagnostics::DoctorReport::capture(
            ctx.cli,
            ctx.cfg,
            paths,
            Some(ctx.session),
            Some(ctx.sandbox),
            false,
        )
        .render(),
        Err(_) => "(diagnostics unavailable)".to_owned(),
    };
    let home = dirs::home_dir().map(|path| path.to_string_lossy().into_owned());
    let body = issue_body(&text, &diagnostics, home.as_deref());
    let title = match text.trim() {
        "" => "Feedback".to_owned(),
        text => text.chars().take(80).collect::<String>().replace('\n', " "),
    };
    let url = issue_url(&title, &body);
    if !open {
        super::write_ok(
            ctx.renderer,
            "Draft issue (nothing is sent; review it, then run /feedback --open [text] to open it \
             in your browser):",
        );
        super::write_result(ctx.renderer, &body);
        return Ok(());
    }
    match crate::ui::renderer::open_url(&url).await {
        Ok(()) => super::write_ok(
            ctx.renderer,
            "opened the prefilled issue in your browser; review it and submit it there",
        ),
        Err(_) => {
            super::write_ok(
                ctx.renderer,
                "could not open a browser; open this address to file the issue:",
            );
            super::write_result(ctx.renderer, &url);
        }
    }
    Ok(())
}

pub(crate) fn handle_release_notes(parts: &[&str], ctx: &mut SlashCtx<'_>) {
    if parts.len() != 1 {
        super::write_usage(ctx.renderer, "usage: /release-notes");
        return;
    }
    super::write_ok(
        ctx.renderer,
        format!("Release notes for mini-agent {}", env!("CARGO_PKG_VERSION")),
    );
    super::write_result(ctx.renderer, release_notes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_has_build_facts_and_no_seeded_secrets_or_home_paths() {
        let diagnostics = "global file: /Users/alice/.mini-agent/config.toml\n\
            api_key: sk-abcdefghijklmnop1234\n\
            note ghp_ABCDEFGHIJKLMNOPQRSTUV and Authorization: Bearer abc.def\n\
            blob 0123456789abcdef0123456789abcdef0123456789\n\
            C:\\Users\\bob\\proj\\x.rs and /home/carol/src";
        let body = issue_body(
            "it crashed in /Users/alice/work token=zzz",
            diagnostics,
            Some("/Users/alice"),
        );
        assert!(body.contains(env!("CARGO_PKG_VERSION")));
        assert!(body.contains(env!("MINI_AGENT_BUILD_TARGET")));
        for secret in [
            "alice",
            "bob",
            "carol",
            "sk-abcdefghijklmnop1234",
            "ghp_ABCDEFGHIJKLMNOPQRSTUV",
            "Bearer abc",
            "0123456789abcdef0123456789abcdef0123456789",
            "zzz",
        ] {
            assert!(!body.contains(secret), "{secret} leaked: {body}");
        }
        assert!(body.contains("~/.mini-agent/config.toml"), "{body}");
    }

    #[test]
    fn issue_url_is_encoded_and_bounded() {
        let url = issue_url("a b&c", &"x".repeat(MAX_BODY_BYTES * 2));
        assert!(
            url.starts_with("https://github.com/sebahrens/mini-agent/issues/new?title=a%20b%26c")
        );
        let body = issue_body(&"y".repeat(20_000), "", None);
        assert!(body.len() <= MAX_BODY_BYTES + 32);
    }

    #[test]
    fn release_notes_name_the_running_version_or_unreleased() {
        let notes = release_notes();
        let first = notes.lines().next().unwrap_or("");
        assert!(
            first.contains(env!("CARGO_PKG_VERSION")) || first.contains("Unreleased"),
            "{first}"
        );
    }
}
