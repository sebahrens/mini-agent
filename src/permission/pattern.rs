use std::borrow::Cow;

use regex::Regex;

#[derive(Debug, Clone)]
pub struct Pattern {
    regex: Regex,
    pub original: String,
    normalize_path_input: bool,
}

impl Pattern {
    pub fn new(pattern: &str) -> Self {
        let original = pattern.to_string();
        let expanded = crate::fs::expand_tilde(pattern);
        Pattern {
            regex: Regex::new(&glob_to_regex(&expanded))
                .expect("glob conversion must always produce a valid regular expression"),
            original,
            normalize_path_input: false,
        }
    }

    pub fn new_regex(pattern: &str) -> Result<Self, regex::Error> {
        let expanded = crate::fs::expand_tilde(pattern);
        Ok(Pattern {
            regex: Regex::new(&expanded)?,
            original: pattern.to_string(),
            // Regex syntax is caller-authored and raw: in particular, a
            // Windows deny regex may intentionally match `\\` separators.
            normalize_path_input: false,
        })
    }

    pub fn matches(&self, input: &str) -> bool {
        self.regex.is_match(input)
    }

    pub fn new_path(pattern: &str) -> Self {
        let mut pattern = Self::new(&normalize_path_separators(pattern));
        pattern.normalize_path_input = true;
        pattern
    }

    /// Decode an opaque literal path scope produced by this module. Keeping
    /// this separate from `new_path` preserves every legacy user-authored glob
    /// meaning, including patterns containing literal bracket characters.
    pub(crate) fn new_generated_path_scope(encoded: &str) -> Option<Self> {
        let (kind, encoded_path) = if let Some(path) = encoded.strip_prefix(EXACT_SCOPE_PREFIX) {
            (GeneratedScopeKind::Exact, path)
        } else if let Some(path) = encoded.strip_prefix(DESCENDANT_SCOPE_PREFIX) {
            (GeneratedScopeKind::Descendants, path)
        } else {
            return None;
        };
        let path = decode_hex_path(encoded_path)?;
        let escaped = regex::escape(&path);
        let regex = match kind {
            GeneratedScopeKind::Exact => format!("^{escaped}$"),
            GeneratedScopeKind::Descendants if path.ends_with('/') => format!("^{escaped}.+$"),
            GeneratedScopeKind::Descendants => format!("^{escaped}/.+$"),
        };
        Some(Self {
            regex: Regex::new(&regex).ok()?,
            original: encoded.to_string(),
            normalize_path_input: true,
        })
    }

    pub fn matches_path(&self, input: &str) -> bool {
        if self.normalize_path_input {
            self.matches(&normalize_path_separators(input))
        } else {
            self.matches(input)
        }
    }
}

const EXACT_SCOPE_PREFIX: &str = "mini-agent-literal-path-v1:exact:";
const DESCENDANT_SCOPE_PREFIX: &str = "mini-agent-literal-path-v1:descendants:";

enum GeneratedScopeKind {
    Exact,
    Descendants,
}

pub(crate) fn normalize_path_separators(path: &str) -> Cow<'_, str> {
    #[cfg(windows)]
    {
        Cow::Owned(normalize_policy_path(path).replace('\\', "/"))
    }
    #[cfg(not(windows))]
    {
        Cow::Borrowed(path)
    }
}

/// Remove Windows' canonicalization-only verbatim prefix while preserving the
/// native separators seen by caller-authored raw regex rules.
#[cfg(any(windows, feature = "lsp"))]
pub(crate) fn normalize_policy_path(path: &str) -> Cow<'_, str> {
    #[cfg(windows)]
    {
        if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
            Cow::Owned(format!(r"\\{rest}"))
        } else if let Some(rest) = path.strip_prefix(r"\\?\") {
            Cow::Owned(rest.to_string())
        } else {
            Cow::Borrowed(path)
        }
    }
    #[cfg(not(windows))]
    {
        Cow::Borrowed(path)
    }
}

pub(crate) fn descendant_path_pattern(path: &std::path::Path) -> String {
    let display = path.to_string_lossy();
    let normalized = normalize_path_separators(&display);
    let root = normalized.trim_end_matches('/');
    encode_generated_scope(
        DESCENDANT_SCOPE_PREFIX,
        if root.is_empty() { "/" } else { root },
    )
}

/// An exact path encoded as a glob without interpreting any of its filename
/// characters as metacharacters. The bracket escapes are standard glob forms
/// and are understood by [`glob_to_regex`].
#[cfg(feature = "lsp")]
pub(crate) fn exact_path_pattern(path: &std::path::Path) -> String {
    let display = path.to_string_lossy();
    let normalized = normalize_path_separators(&display);
    encode_generated_scope(EXACT_SCOPE_PREFIX, &normalized)
}

fn encode_generated_scope(prefix: &str, path: &str) -> String {
    let mut encoded = String::with_capacity(prefix.len() + path.len() * 2);
    encoded.push_str(prefix);
    for byte in path.as_bytes() {
        use std::fmt::Write;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn decode_hex_path(encoded: &str) -> Option<String> {
    if !encoded.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(encoded.len() / 2);
    for pair in encoded.as_bytes().chunks_exact(2) {
        let pair = std::str::from_utf8(pair).ok()?;
        bytes.push(u8::from_str_radix(pair, 16).ok()?);
    }
    String::from_utf8(bytes).ok()
}

fn glob_to_regex(pattern: &str) -> String {
    let mut re = String::with_capacity(pattern.len() * 2);
    re.push('^');
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        re.push_str("(?:.*/)?");
                    } else {
                        re.push_str(".*");
                    }
                } else {
                    re.push_str("[^/]*");
                }
            }
            '?' => re.push('.'),
            '.' => re.push_str("\\."),
            '\\' => re.push_str("\\\\"),
            '(' | ')' | '[' | ']' | '{' | '}' | '+' | '^' | '$' | '|' => {
                re.push('\\');
                re.push(c);
            }
            _ => re.push(c),
        }
    }
    re.push('$');
    re
}
