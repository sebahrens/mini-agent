//! Bounded single-pass lexical highlighting; never loads or executes grammars.
//! Spans describe foregrounds only. The original row remains the copy/selection source.
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use super::feed::ForegroundPalette;
use crossterm::style::Color;

pub(crate) const MAX_HIGHLIGHT_BYTES: usize = 32 * 1024;
const MAX_HIGHLIGHT_LINES: usize = 512;
const MAX_LINE_BYTES: usize = 2048;
const MAX_SPANS: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TokenSpan {
    pub range: Range<usize>,
    pub color: Color,
}

#[cfg(test)]
thread_local! {
    static PARSED_LINES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SCAN_STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
#[cfg(test)]
pub(crate) fn parsed_lines_for_test() -> usize {
    PARSED_LINES.with(std::cell::Cell::get)
}
#[cfg(test)]
fn scan_steps_for_test() -> usize {
    SCAN_STEPS.with(std::cell::Cell::get)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Language {
    Rust,
    C,
    JavaScript,
    Python,
    Json,
    Toml,
    Shell,
    Diff,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Quoted {
    None,
    Single(u8),
    Triple(u8),
    RustRaw(u8),
}

/// Exact state at a completed line, including the admitted work budget. No
/// recursive parser/regex/backtracking stack is involved in cloning or scanning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CodeState {
    language: Language,
    quote: Quoted,
    comment_depth: u16,
    bytes: usize,
    lines: usize,
    spans: usize,
    disabled: bool,
}

#[derive(Clone, Debug)]
struct CachedLine {
    text: String,
    after: CodeState,
    spans: Option<Arc<[TokenSpan]>>,
}
/// Width-independent cache owned by one feed block. Exact source and initial
/// lexical state must match; identical text in a different comment/string is
/// never reused. Theme/monochrome changes discard it with the row caches.
#[derive(Clone, Debug, Default)]
pub(crate) struct TokenCache {
    lines: HashMap<CodeState, Vec<CachedLine>>,
    bytes: usize,
    count: usize,
}
impl TokenCache {
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }
}

impl CodeState {
    pub(crate) fn new(language: &str) -> Option<Self> {
        let token = language.split_whitespace().next()?;
        if token.len() > 64 {
            return None;
        }
        let language = match token.to_ascii_lowercase().as_str() {
            "rust" | "rs" => Language::Rust,
            "c" | "cpp" | "c++" | "h" | "java" | "go" => Language::C,
            "javascript" | "js" | "node" | "typescript" | "ts" | "jsx" | "tsx" => {
                Language::JavaScript
            }
            "python" | "py" => Language::Python,
            "json" | "jsonc" => Language::Json,
            "toml" => Language::Toml,
            "sh" | "shell" | "bash" | "zsh" => Language::Shell,
            "diff" | "patch" => Language::Diff,
            _ => return None,
        };
        Some(Self {
            language,
            quote: Quoted::None,
            comment_depth: 0,
            bytes: 0,
            lines: 0,
            spans: 0,
            disabled: false,
        })
    }

    pub(crate) fn cached_line(
        &mut self,
        line: &str,
        palette: ForegroundPalette,
        cache: &mut TokenCache,
    ) -> Option<Arc<[TokenSpan]>> {
        let before = *self;
        if let Some(cached) = cache
            .lines
            .get(&before)
            .and_then(|rows| rows.iter().find(|row| row.text == line))
        {
            *self = cached.after;
            return cached.spans.clone();
        }
        let spans = self.line(line, palette);
        if cache.bytes.saturating_add(line.len()) <= MAX_HIGHLIGHT_BYTES
            && cache.count < MAX_HIGHLIGHT_LINES
        {
            cache.bytes += line.len();
            cache.count += 1;
            cache.lines.entry(before).or_default().push(CachedLine {
                text: line.to_owned(),
                after: *self,
                spans: spans.clone(),
            });
        }
        spans
    }

    pub(crate) fn line(
        &mut self,
        line: &str,
        palette: ForegroundPalette,
    ) -> Option<Arc<[TokenSpan]>> {
        self.bytes = self.bytes.saturating_add(line.len());
        self.lines = self.lines.saturating_add(1);
        if self.disabled
            || self.bytes > MAX_HIGHLIGHT_BYTES
            || self.lines > MAX_HIGHLIGHT_LINES
            || line.len() > MAX_LINE_BYTES
        {
            self.disabled = true;
            return None;
        }
        let display = line
            .strip_suffix('\n')
            .unwrap_or(line)
            .trim_end_matches('\r');
        if display.is_empty() {
            return None;
        }
        #[cfg(test)]
        {
            PARSED_LINES.with(|n| n.set(n.get() + 1));
        }
        if self.language == Language::Diff {
            let color = if ["+++", "---", "@@", "diff ", "index "]
                .iter()
                .any(|p| display.starts_with(p))
            {
                palette.link
            } else if display.starts_with('+') {
                palette.diff_add
            } else if display.starts_with('-') {
                palette.diff_del
            } else {
                palette.code
            };
            self.spans += 1;
            return Some(
                vec![TokenSpan {
                    range: 0..display.len(),
                    color,
                }]
                .into(),
            );
        }
        let bytes = display.as_bytes();
        let mut pos = 0;
        let mut spans: Vec<TokenSpan> = Vec::new();
        while pos < bytes.len() {
            scan_step();
            let start = pos;
            let color;
            if self.comment_depth > 0 {
                while pos < bytes.len() {
                    scan_step();
                    if bytes[pos..].starts_with(b"*/") {
                        self.comment_depth -= 1;
                        pos += 2;
                        if self.comment_depth == 0 {
                            break;
                        }
                    } else if self.language == Language::Rust && bytes[pos..].starts_with(b"/*") {
                        self.comment_depth = self.comment_depth.saturating_add(1);
                        pos += 2;
                    } else {
                        pos += display[pos..].chars().next().unwrap().len_utf8();
                    }
                }
                color = palette.system;
            } else if self.quote != Quoted::None {
                self.scan_quote(display, &mut pos);
                color = palette.diff_add;
            } else if self.slash_comments() && bytes[pos..].starts_with(b"//")
                || self.hash_comments() && bytes[pos] == b'#'
            {
                pos = bytes.len();
                color = palette.system;
            } else if self.slash_comments() && bytes[pos..].starts_with(b"/*") {
                self.comment_depth = 1;
                pos += 2;
                // Re-enter the loop with the comment opener included in its span.
                push_span(&mut spans, start..pos, palette.system);
                if self.spans.saturating_add(spans.len()) > MAX_SPANS {
                    self.disabled = true;
                    return None;
                }
                continue;
            } else if self.language == Language::Rust
                && bytes[pos] == b'r'
                && self.open_rust_raw(bytes, &mut pos)
            {
                self.scan_quote(display, &mut pos);
                color = palette.diff_add;
            } else if matches!(bytes[pos], b'"' | b'\'' | b'`') && self.accept_quote(display, pos) {
                let quote = bytes[pos];
                let triple = matches!(self.language, Language::Python | Language::Toml)
                    && bytes[pos..].starts_with(&[quote; 3]);
                self.quote = if triple {
                    Quoted::Triple(quote)
                } else {
                    Quoted::Single(quote)
                };
                pos += if triple { 3 } else { 1 };
                self.scan_quote(display, &mut pos);
                color = palette.diff_add;
            } else if bytes[pos].is_ascii_digit() {
                pos += 1;
                while pos < bytes.len()
                    && (bytes[pos].is_ascii_alphanumeric() || matches!(bytes[pos], b'_' | b'.'))
                {
                    scan_step();
                    pos += 1;
                }
                color = palette.warning;
            } else if identifier_start(display[pos..].chars().next().unwrap()) {
                pos += display[pos..].chars().next().unwrap().len_utf8();
                while pos < bytes.len() {
                    scan_step();
                    let ch = display[pos..].chars().next().unwrap();
                    if !identifier_start(ch) && !ch.is_ascii_digit() {
                        break;
                    }
                    pos += ch.len_utf8();
                }
                color = if is_keyword(self.language, &display[start..pos]) {
                    palette.link
                } else {
                    palette.code
                };
            } else {
                pos += display[pos..].chars().next().unwrap().len_utf8();
                color = palette.code;
            }
            push_span(&mut spans, start..pos, color);
            if self.spans.saturating_add(spans.len()) > MAX_SPANS {
                self.disabled = true;
                return None;
            }
        }
        // Ordinary C-family string/char literals end at a physical newline.
        // Backticks, shell quotes, raw strings and triple quotes retain state.
        if let Quoted::Single(q) = self.quote
            && q != b'`'
            && matches!(
                self.language,
                Language::C | Language::JavaScript | Language::Python | Language::Json
            )
            && line.ends_with('\n')
            && !display.ends_with('\\')
        {
            self.quote = Quoted::None;
        }
        self.spans += spans.len();
        Some(spans.into())
    }
    fn slash_comments(self) -> bool {
        matches!(
            self.language,
            Language::Rust | Language::C | Language::JavaScript | Language::Json
        )
    }
    fn hash_comments(self) -> bool {
        matches!(
            self.language,
            Language::Python | Language::Toml | Language::Shell
        )
    }
    fn accept_quote(self, text: &str, pos: usize) -> bool {
        let q = text.as_bytes()[pos];
        if q == b'`' {
            return self.language == Language::JavaScript;
        }
        // Rust lifetimes are identifiers, not unclosed character literals.
        if self.language == Language::Rust && q == b'\'' {
            return rust_character_literal(text, pos);
        }
        q != b'\'' || self.language != Language::Json
    }
    fn open_rust_raw(&mut self, bytes: &[u8], pos: &mut usize) -> bool {
        let mut end = *pos + 1;
        while end < bytes.len() && bytes[end] == b'#' && end - *pos <= 16 {
            end += 1;
        }
        if end < bytes.len() && bytes[end] == b'"' {
            self.quote = Quoted::RustRaw((end - *pos - 1) as u8);
            *pos = end + 1;
            true
        } else {
            false
        }
    }
    fn scan_quote(&mut self, text: &str, pos: &mut usize) {
        let bytes = text.as_bytes();
        while *pos < bytes.len() {
            scan_step();
            let close = match self.quote {
                Quoted::Single(q) => bytes[*pos] == q,
                Quoted::Triple(q) => bytes[*pos..].starts_with(&[q; 3]),
                Quoted::RustRaw(hashes) => {
                    bytes[*pos] == b'"'
                        && bytes
                            .get(*pos + 1..*pos + 1 + hashes as usize)
                            .is_some_and(|s| s.iter().all(|b| *b == b'#'))
                }
                Quoted::None => return,
            };
            if close {
                *pos += match self.quote {
                    Quoted::Triple(_) => 3,
                    Quoted::RustRaw(h) => 1 + h as usize,
                    _ => 1,
                };
                self.quote = Quoted::None;
                return;
            }
            if bytes[*pos] == b'\\'
                && !matches!(self.quote, Quoted::RustRaw(_))
                && !(self.quote == Quoted::Single(b'\'')
                    && matches!(self.language, Language::Shell | Language::Toml))
            {
                *pos += 1;
                if *pos < bytes.len() {
                    *pos += text[*pos..].chars().next().unwrap().len_utf8();
                }
            } else {
                *pos += text[*pos..].chars().next().unwrap().len_utf8();
            }
        }
    }
}
#[inline]
fn scan_step() {
    #[cfg(test)]
    SCAN_STEPS.with(|n| n.set(n.get() + 1));
}
/// One scalar or one bounded escape must be immediately followed by the closing
/// apostrophe. Searching for a later apostrophe would swallow adjacent lifetimes.
fn rust_character_literal(text: &str, pos: usize) -> bool {
    let mut chars = text[pos + 1..].chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first == '\\' {
        match chars.next() {
            Some('n' | 'r' | 't' | '0' | '\\' | '\'' | '"') => {}
            Some('x') => {
                for _ in 0..2 {
                    if !chars.next().is_some_and(|ch| ch.is_ascii_hexdigit()) {
                        return false;
                    }
                }
            }
            Some('u') => {
                if chars.next() != Some('{') {
                    return false;
                }
                let mut digits = 0;
                let mut closed = false;
                // Fixed lookahead preserves the scanner's bound; uncommon
                // overlong escapes retain plain styling rather than guessing.
                for _ in 0..16 {
                    match chars.next() {
                        Some('}') => {
                            closed = true;
                            break;
                        }
                        Some('_') => {}
                        Some(ch) if ch.is_ascii_hexdigit() => {
                            digits += 1;
                            if digits > 6 {
                                return false;
                            }
                        }
                        _ => return false,
                    }
                }
                if !closed || digits == 0 {
                    return false;
                }
            }
            _ => return false,
        }
    } else if matches!(first, '\'' | '\n' | '\r' | '\t') {
        return false;
    }
    chars.next() == Some('\'')
}

fn identifier_start(ch: char) -> bool {
    ch == '_' || ch.is_alphabetic()
}
fn push_span(spans: &mut Vec<TokenSpan>, range: Range<usize>, color: Color) {
    if let Some(last) = spans
        .last_mut()
        .filter(|p| p.color == color && p.range.end == range.start)
    {
        last.range.end = range.end;
    } else {
        spans.push(TokenSpan { range, color });
    }
}
fn is_keyword(language: Language, word: &str) -> bool {
    let common = matches!(word, "true" | "false" | "null");
    common
        || match language {
            Language::Rust => matches!(
                word,
                "fn" | "let"
                    | "mut"
                    | "pub"
                    | "impl"
                    | "struct"
                    | "enum"
                    | "use"
                    | "mod"
                    | "match"
                    | "if"
                    | "else"
                    | "for"
                    | "while"
                    | "loop"
                    | "return"
                    | "async"
                    | "await"
                    | "move"
                    | "self"
                    | "Self"
                    | "trait"
                    | "const"
                    | "static"
                    | "type"
                    | "where"
                    | "unsafe"
                    | "in"
                    | "ref"
                    | "as"
                    | "break"
                    | "continue"
            ),
            Language::C => matches!(
                word,
                "int"
                    | "void"
                    | "char"
                    | "float"
                    | "double"
                    | "bool"
                    | "class"
                    | "struct"
                    | "public"
                    | "private"
                    | "static"
                    | "const"
                    | "auto"
                    | "new"
                    | "package"
                    | "func"
                    | "var"
                    | "import"
                    | "if"
                    | "else"
                    | "for"
                    | "while"
                    | "return"
                    | "switch"
                    | "case"
                    | "break"
                    | "continue"
            ),
            Language::JavaScript => matches!(
                word,
                "const"
                    | "let"
                    | "var"
                    | "function"
                    | "return"
                    | "class"
                    | "new"
                    | "if"
                    | "else"
                    | "for"
                    | "while"
                    | "switch"
                    | "case"
                    | "break"
                    | "continue"
                    | "async"
                    | "await"
                    | "import"
                    | "export"
                    | "from"
                    | "interface"
                    | "type"
                    | "extends"
                    | "this"
                    | "throw"
                    | "try"
                    | "catch"
            ),
            Language::Python => matches!(
                word,
                "def"
                    | "class"
                    | "import"
                    | "from"
                    | "as"
                    | "if"
                    | "elif"
                    | "else"
                    | "for"
                    | "while"
                    | "in"
                    | "return"
                    | "yield"
                    | "with"
                    | "try"
                    | "except"
                    | "finally"
                    | "raise"
                    | "pass"
                    | "lambda"
                    | "async"
                    | "await"
                    | "None"
                    | "True"
                    | "False"
                    | "and"
                    | "or"
                    | "not"
            ),
            Language::Shell => matches!(
                word,
                "if" | "then"
                    | "else"
                    | "elif"
                    | "fi"
                    | "for"
                    | "while"
                    | "do"
                    | "done"
                    | "case"
                    | "esac"
                    | "in"
                    | "function"
                    | "export"
                    | "local"
            ),
            Language::Json | Language::Toml | Language::Diff => false,
        }
}
pub(crate) fn within_budget(text: &str) -> bool {
    text.len() <= MAX_HIGHLIGHT_BYTES
        && text
            .split_inclusive('\n')
            .take(MAX_HIGHLIGHT_LINES + 1)
            .count()
            <= MAX_HIGHLIGHT_LINES
        && text
            .split_inclusive('\n')
            .all(|line| line.len() <= MAX_LINE_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn syntax_rust_roles_and_multiline_state_are_theme_resolved() {
        let palette = ForegroundPalette::default();
        let mut state = CodeState::new("rust").unwrap();
        let first = state.line("/* started\n", palette).unwrap();
        assert!(first.iter().all(|s| s.color == palette.system));
        let mut copied = state;
        let second = copied
            .line("still comment */ let x = \"雪\";\n", palette)
            .unwrap();
        assert_eq!(second[0].color, palette.system);
        assert!(second.iter().any(|s| s.color == palette.diff_add));
        let mut different = palette;
        different.diff_add = Color::Rgb {
            r: 20,
            g: 200,
            b: 180,
        };
        let colored = state.line("*/ let x = \"雪\";\n", different).unwrap();
        assert!(colored.iter().any(|s| s.color == different.diff_add));
        for s in &*colored {
            assert!("*/ let x = \"雪\";".is_char_boundary(s.range.start));
            assert!("*/ let x = \"雪\";".is_char_boundary(s.range.end));
        }
    }
    #[test]
    fn syntax_unknown_and_large_inputs_fall_back_before_parsing() {
        assert!(CodeState::new("").is_none());
        assert!(CodeState::new("unknown-mini-agent-language").is_none());
        assert!(!within_budget(&"let x = 1;\n".repeat(10_000)));
        assert!(!within_budget(&"雪".repeat(MAX_LINE_BYTES)));
        let mut state = CodeState::new("rust").unwrap();
        assert!(
            state
                .line(
                    &"x".repeat(MAX_LINE_BYTES + 1),
                    ForegroundPalette::default()
                )
                .is_none()
        );
        assert!(
            state
                .line("let x=1;", ForegroundPalette::default())
                .is_none()
        );
    }
    #[test]
    fn syntax_diff_headers_additions_and_deletions_keep_semantic_roles() {
        let palette = ForegroundPalette::default();
        let mut state = CodeState::new("diff").unwrap();
        for (line, color) in [
            ("+++ b/file\n", palette.link),
            ("@@ -1 +1 @@\n", palette.link),
            ("+雪\n", palette.diff_add),
            ("-old\n", palette.diff_del),
        ] {
            let spans = state.line(line, palette).unwrap();
            assert_eq!(spans[0].color, color);
            assert_eq!(spans[0].range.end, line.trim_end_matches('\n').len());
        }
    }
    #[test]
    fn syntax_raw_nested_comments_and_triple_strings_keep_exact_line_state() {
        let p = ForegroundPalette::default();
        let mut rust = CodeState::new("rust").unwrap();
        rust.line("let text = r##\"雪\n", p);
        let raw = rust.line("\"# remains raw\"##; let n=42;\n", p).unwrap();
        assert_eq!(raw[0].color, p.diff_add);
        assert!(raw.iter().any(|s| s.color == p.warning));
        rust.line("/* outer /* inner\n", p);
        assert!(
            rust.line("*/ still outer\n", p)
                .unwrap()
                .iter()
                .all(|s| s.color == p.system)
        );
        assert!(
            rust.line("*/ fn main() {}\n", p)
                .unwrap()
                .iter()
                .any(|s| s.color == p.link)
        );
        let mut py = CodeState::new("python").unwrap();
        py.line("s = '''hello\n", p);
        assert!(
            py.line("# not a comment'''\n", p)
                .unwrap()
                .iter()
                .all(|s| s.color == p.diff_add)
        );
    }
    #[test]
    fn syntax_common_languages_keep_strings_comments_numbers_and_keywords() {
        let p = ForegroundPalette::default();
        for (language, text) in [
            ("js", "const x = \"雪\"; // comment"),
            ("python", "def fn(): x = \"雪\"; # comment"),
            ("sh", "if x=42; then echo \"雪\"; # comment"),
            ("cpp", "int x=42; /* comment */ \"雪\""),
            ("json", "{\"雪\": 42, \"ok\": true}"),
            ("toml", "x=42 # comment"),
        ] {
            let mut state = CodeState::new(language).unwrap();
            let spans = state.line(text, p).unwrap();
            if language != "toml" {
                assert!(spans.iter().any(|s| s.color == p.diff_add), "{language}");
            }
            if language != "js" && language != "python" {
                assert!(spans.iter().any(|s| s.color == p.warning), "{language}");
            }
            if language != "json" {
                assert!(spans.iter().any(|s| s.color == p.system), "{language}");
            }
        }
        let mut rust = CodeState::new("rust").unwrap();
        rust.line("let s=\"first\n", p);
        assert!(
            rust.line("still snow 雪\";", p)
                .unwrap()
                .iter()
                .any(|s| s.color == p.diff_add)
        );
        let mut js = CodeState::new("js").unwrap();
        js.line("const s=`first\n", p);
        assert_eq!(js.line("// literal`", p).unwrap()[0].color, p.diff_add);
        let mut rust = CodeState::new("rust").unwrap();
        assert!(
            rust.line("let c='\\\"'; let lifetime: &'a str;", p)
                .is_some()
        );
        let lifetime_text = "fn f<'a, 'b>(a: &'a str, b: &'b str) {}";
        let spans = CodeState::new("rust")
            .unwrap()
            .line(lifetime_text, p)
            .unwrap();
        let false_strings: Vec<_> = spans
            .iter()
            .filter(|span| span.color == p.diff_add)
            .map(|span| &lifetime_text[span.range.clone()])
            .collect();
        assert!(
            false_strings.is_empty(),
            "lifetimes incorrectly colored as strings: {false_strings:?}"
        );
        for literal in [
            "'a'",
            "'雪'",
            "'🙂'",
            "' '",
            r"'\n'",
            r"'\''",
            r"'\x7f'",
            r"'\u{1F642}'",
        ] {
            let text = format!("let c={literal};");
            let spans = CodeState::new("rust").unwrap().line(&text, p).unwrap();
            assert!(
                spans
                    .iter()
                    .any(|span| span.color == p.diff_add && span.range == (6..6 + literal.len())),
                "character literal must remain highlighted: {literal}"
            );
        }
    }
    #[test]
    fn syntax_span_and_cache_retention_budgets_have_plain_fallbacks() {
        let p = ForegroundPalette::default();
        let mut state = CodeState::new("rust").unwrap();
        let alternating = format!("{}\n", "\"x\"1".repeat(400));
        for _ in 0..5 {
            assert!(state.line(&alternating, p).is_some());
        }
        assert!(state.line(&alternating, p).is_none());
        assert!(state.line("fn main() {}\n", p).is_none());
        let mut cache = TokenCache::default();
        for i in 0..600 {
            let mut s = CodeState::new("rust").unwrap();
            s.cached_line(&format!("let n={i};\n"), p, &mut cache);
        }
        assert!(cache.count <= MAX_HIGHLIGHT_LINES);
        assert!(cache.bytes <= MAX_HIGHLIGHT_BYTES);
        cache.clear();
        assert!(cache.lines.is_empty());
    }
    #[test]
    fn syntax_adversarial_tokens_are_single_pass_and_cache_is_state_sensitive() {
        let p = ForegroundPalette::default();
        let line = format!("{}\n", "\\\"/*雪".repeat(200));
        let before = scan_steps_for_test();
        let mut state = CodeState::new("rust").unwrap();
        state.line(&line, p);
        let steps = scan_steps_for_test() - before;
        assert!(
            steps > 0 && steps <= 2 * line.len(),
            "bounded advancing scan: {steps}"
        );
        let mut cache = TokenCache::default();
        let mut a = CodeState::new("rust").unwrap();
        let initial = a;
        let first = a.cached_line("let x=1;\n", p, &mut cache);
        let calls = parsed_lines_for_test();
        let mut same = initial;
        assert_eq!(same.cached_line("let x=1;\n", p, &mut cache), first);
        assert_eq!(parsed_lines_for_test(), calls);
        let mut comment = initial;
        comment.comment_depth = 1;
        assert!(
            comment
                .cached_line("let x=1;\n", p, &mut cache)
                .unwrap()
                .iter()
                .all(|s| s.color == p.system)
        );
        assert_eq!(parsed_lines_for_test(), calls + 1);
    }
}
