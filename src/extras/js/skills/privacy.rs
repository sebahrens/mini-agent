//! Shared privacy primitives for bounded redaction of learned-skill telemetry.

use std::sync::LazyLock;

use regex::Regex;

/// Whole PEM private-key blocks, including the multi-line body.
static PRIVATE_KEY_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----")
        .expect("static private key block regex")
});

/// Bare or quoted credential assignments. Quoted values include whitespace,
/// punctuation, and escaped quotes; an unfinished quote consumes the remainder
/// of the input. The scheme prefix also covers `Authorization: Bearer <token>`.
static LABELED_CREDENTIAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r#"(?i)(?P<prefix>["']?\b(?:api[_-]?key|apikey|access[_-]?token|"#,
        r#"refresh[_-]?token|id[_-]?token|client[_-]?secret|private[_-]?key|"#,
        r#"token|password|passwd|secret|authorization)\b["']?\s*[:=]\s*)"#,
        r#"(?P<value>"(?:\\(?s:.|$)|[^"\\])*(?:"|$)|"#,
        r#"'(?:\\(?s:.|$)|[^'\\])*(?:'|$)|"#,
        r#"(?:bearer\s+|basic\s+|token\s+)?[^"',;\s}\[\]]+)"#,
    ))
    .expect("static labeled credential regex")
});

/// Bare `Bearer <token>` with no preceding label.
static BEARER_CREDENTIAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9\-._~+/]{8,}={0,2}").expect("static bearer regex")
});

/// Vendor token shapes that carry no label at all.
static PREFIXED_CREDENTIAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:sk-[A-Za-z0-9_-]{16,}|rk-[A-Za-z0-9_-]{16,}|AKIA[0-9A-Z]{16}|ASIA[0-9A-Z]{16}|gh[pousr]_[A-Za-z0-9]{16,}|github_pat_[A-Za-z0-9_]{20,}|xox[abprs]-[A-Za-z0-9-]{10,}|AIza[0-9A-Za-z_-]{35}|eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,})",
    )
    .expect("static prefixed credential regex")
});

#[derive(Debug, Clone)]
pub struct Redactor {
    exact_secrets: Vec<String>,
    max_bytes: usize,
}

impl Redactor {
    pub fn new(exact_secrets: Vec<String>, max_bytes: usize) -> Self {
        let mut exact_secrets: Vec<String> = exact_secrets
            .into_iter()
            .filter(|secret| !secret.is_empty())
            .collect();
        exact_secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        exact_secrets.dedup();
        Self {
            exact_secrets,
            max_bytes,
        }
    }

    pub fn redact(&self, input: &str) -> String {
        let mut value = input.to_string();
        for secret in &self.exact_secrets {
            value = value.replace(secret, "[REDACTED]");
        }
        // Redact common credential shapes without retaining the value. The
        // block form runs first so a labelled `private_key: -----BEGIN ...`
        // cannot leave the body behind.
        value = PRIVATE_KEY_BLOCK
            .replace_all(&value, "[REDACTED PRIVATE KEY]")
            .into_owned();
        value = LABELED_CREDENTIAL
            .replace_all(&value, |captures: &regex::Captures<'_>| {
                let prefix = &captures["prefix"];
                let value = &captures["value"];
                match value.as_bytes()[0] {
                    b'"' => format!("{prefix}\"[REDACTED]\""),
                    b'\'' => format!("{prefix}'[REDACTED]'"),
                    _ => format!("{prefix}[REDACTED]"),
                }
            })
            .into_owned();
        value = BEARER_CREDENTIAL
            .replace_all(&value, "bearer [REDACTED]")
            .into_owned();
        value = PREFIXED_CREDENTIAL
            .replace_all(&value, "[REDACTED]")
            .into_owned();
        truncate_utf8(&value, self.max_bytes)
    }

    pub fn contains_configured_secret(&self, value: &str) -> bool {
        self.exact_secrets
            .iter()
            .any(|secret| value.contains(secret))
    }
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value[..end].to_string()
}
