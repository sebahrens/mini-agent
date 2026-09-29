use std::borrow::Cow;

use regex::Regex;

#[derive(Debug, Clone)]
pub struct Pattern {
    regex: Regex,
    /// Case-insensitive twin of `regex`, used only for deny decisions on
    /// platforms whose default volumes are case-insensitive (macOS, Windows),
    /// where `.ENV` opens the same file as `.env`.
    #[cfg(any(target_os = "macos", windows))]
    folded_regex: Regex,
    pub original: String,
    normalize_path_input: bool,
    /// Number of literal (non-wildcard) characters in the pattern. Rule
    /// resolution prefers the most specific matching pattern; see
    /// `docs/agent/CONFIG.md` ("Rule precedence").
    specificity: usize,
    /// True when the pattern can only match absolute paths it names
    /// explicitly (`/opt/**`, `~/src/**`, `^/opt/`), as opposed to a
    /// relative or match-anything pattern such as `**/*.rs` or `src/**`.
    absolute_anchored: bool,
}

impl Pattern {
    pub fn new(pattern: &str) -> Self {
        let original = pattern.to_string();
        let expanded = crate::fs::expand_tilde(pattern);
        Pattern {
            regex: Regex::new(&glob_to_regex(&expanded))
                .expect("glob conversion must always produce a valid regular expression"),
            #[cfg(any(target_os = "macos", windows))]
            folded_regex: Regex::new(&format!("(?i){}", glob_to_regex(&expanded)))
                .expect("glob conversion must always produce a valid regular expression"),
            specificity: glob_specificity(&expanded),
            absolute_anchored: is_absolute_path_text(&expanded),
            original,
            normalize_path_input: false,
        }
    }

    pub fn new_regex(pattern: &str) -> Result<Self, regex::Error> {
        let expanded = crate::fs::expand_tilde(pattern);
        // `(?s)` makes `.` match newlines so a multi-line input can never
        // slip past an anchored deny rule. Caller flags such as `(?i)` remain
        // valid after the prefix.
        Ok(Pattern {
            regex: Regex::new(&format!("(?s){expanded}"))?,
            #[cfg(any(target_os = "macos", windows))]
            folded_regex: Regex::new(&format!("(?i)(?s){expanded}"))?,
            specificity: regex_specificity(&expanded),
            absolute_anchored: expanded.strip_prefix('^').is_some_and(|rest| {
                // A regex backslash starts an escape, so a UNC root is `\\\\` (two escaped backslashes).
                rest.starts_with('/')
                    || rest.starts_with(r"\\\\")
                    || (!rest.starts_with('\\') && is_absolute_path_text(rest))
            }),
            original: pattern.to_string(),
            // Regex syntax is caller-authored and raw: in particular, a
            // Windows deny regex may intentionally match `\\` separators.
            normalize_path_input: false,
        })
    }

    /// Whether this rule names an absolute location rather than matching
    /// relative spellings or everything. Only anchored allow rules may grant
    /// access outside the workspace without an `external_directory` allow.
    pub fn is_absolute_anchored(&self) -> bool {
        self.absolute_anchored
    }

    /// Literal-character count used for deterministic rule precedence.
    pub fn specificity(&self) -> usize {
        self.specificity
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
            #[cfg(any(target_os = "macos", windows))]
            folded_regex: Regex::new(&format!("(?i){regex}")).ok()?,
            regex: Regex::new(&regex).ok()?,
            specificity: path.chars().count(),
            absolute_anchored: true,
            original: encoded.to_string(),
            normalize_path_input: true,
        })
    }

    /// Path match used for deny rules: on macOS and Windows it also matches
    /// any ASCII/Unicode case variant, because the default volumes there open
    /// `Secrets/key` as the same file as `secrets/key`.
    pub fn matches_path_for_deny(&self, input: &str) -> bool {
        if self.matches_path(input) {
            return true;
        }
        #[cfg(any(target_os = "macos", windows))]
        {
            if self.normalize_path_input {
                self.folded_regex
                    .is_match(&normalize_path_separators(input))
            } else {
                self.folded_regex.is_match(input)
            }
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            false
        }
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
pub(crate) fn exact_path_pattern(path: &std::path::Path) -> String {
    let display = path.to_string_lossy();
    let normalized = normalize_path_separators(&display);
    encode_generated_scope(EXACT_SCOPE_PREFIX, &normalized)
}

/// The AllowAlways scope for one approved directory-walking search root
/// (`grep`, `find_files`): the literal tree under `root` plus `root` itself,
/// returned as `(descendants, exact)`. Both are opaque literal scopes, so a
/// root is never widened to a sibling that merely shares its prefix and a
/// root containing whitespace or glob metacharacters is kept whole.
pub(crate) fn search_root_allow_scope(root: &str) -> (String, String) {
    let expanded = crate::fs::expand_tilde(root);
    let root = std::path::Path::new(&expanded);
    (descendant_path_pattern(root), exact_path_pattern(root))
}

/// The human-readable form of an AllowAlways pattern, for showing the user
/// exactly what a grant covers. Generated literal scopes are decoded (they
/// are opaque hex on the wire); any other pattern is returned verbatim.
#[cfg_attr(not(feature = "acp"), allow(dead_code))]
pub(crate) fn describe_allow_pattern(pattern: &str) -> String {
    let (kind, encoded) = if let Some(encoded) = pattern.strip_prefix(EXACT_SCOPE_PREFIX) {
        (GeneratedScopeKind::Exact, encoded)
    } else if let Some(encoded) = pattern.strip_prefix(DESCENDANT_SCOPE_PREFIX) {
        (GeneratedScopeKind::Descendants, encoded)
    } else {
        return pattern.to_string();
    };
    let Some(path) = decode_hex_path(encoded) else {
        return pattern.to_string();
    };
    match kind {
        GeneratedScopeKind::Exact => format!("exactly {path}"),
        GeneratedScopeKind::Descendants if path.ends_with('/') => format!("anything under {path}"),
        GeneratedScopeKind::Descendants => format!("anything under {path}/"),
    }
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

/// `/…`, `\\…`, or a Windows drive prefix such as `C:`.
fn is_absolute_path_text(text: &str) -> bool {
    let bytes = text.as_bytes();
    text.starts_with('/')
        || text.starts_with('\\')
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

/// Count the literal characters of a glob: everything except `*` and `?`.
fn glob_specificity(pattern: &str) -> usize {
    pattern.chars().filter(|c| !matches!(c, '*' | '?')).count()
}

/// Count the literal characters of a regex: everything except metacharacters
/// and escape backslashes. This is a deterministic heuristic for precedence,
/// not a full regex analysis.
fn regex_specificity(pattern: &str) -> usize {
    pattern
        .chars()
        .filter(|c| {
            !matches!(
                c,
                '.' | '*' | '+' | '?' | '^' | '$' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\'
            )
        })
        .count()
}

fn glob_to_regex(pattern: &str) -> String {
    let mut re = String::with_capacity(pattern.len() * 2 + 4);
    // Single-line mode: `.` matches `\n`, so an embedded newline can never
    // make an anchored deny pattern miss (see `multiline_bash_script_*` tests).
    re.push_str("(?s)^");
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
