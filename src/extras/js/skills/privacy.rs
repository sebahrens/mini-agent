//! Shared privacy primitives for opaque correlation and bounded redaction.

use sha2::{Digest, Sha256};

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
        // Redact common credential assignments without retaining the value.
        let credential = regex::Regex::new(
            r#"(?i)(api[_-]?key|token|password|secret|authorization)\s*[:=]\s*["']?[^"',\s}\]]+"#,
        )
        .expect("static credential regex");
        value = credential
            .replace_all(&value, |captures: &regex::Captures<'_>| {
                let label = captures.get(1).map_or("secret", |value| value.as_str());
                format!("{label}=[REDACTED]")
            })
            .into_owned();
        truncate_utf8(&value, self.max_bytes)
    }

    pub fn contains_configured_secret(&self, value: &str) -> bool {
        self.exact_secrets
            .iter()
            .any(|secret| value.contains(secret))
    }
}

pub fn keyed_fingerprint(key: &[u8], version: &str, value: &str) -> Option<String> {
    if key.is_empty() || version.is_empty() {
        return None;
    }
    let mut digest = Sha256::new();
    digest.update(b"mini-agent/private-fingerprint/v1");
    for part in [key, version.as_bytes(), value.as_bytes()] {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    Some(format!("{version}:{:x}", digest.finalize()))
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
