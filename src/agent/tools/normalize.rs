/// Whitespace-normalized text together with a byte-level map back to the
/// source it was derived from.
///
/// Normalization expands tabs to four spaces, trims trailing whitespace from
/// every line, collapses runs of blank lines to a single blank line, and
/// terminates every line (including an unterminated last line) with `\n`.
/// Because bytes are added, dropped, and merged, a match found in the
/// normalized text cannot be mapped back to the source by arithmetic; use
/// [`NormalizedText::source_range`] instead.
pub struct NormalizedText {
    pub text: String,
    /// `source_offsets[i]` is the source byte offset that produced normalized
    /// byte `i`. A tab's four spaces all map to the tab byte; a line's `\n`
    /// maps to the start of its source terminator (`\r` for CRLF, `\n` for LF),
    /// or to the source length for a synthetic terminator on an unterminated last
    /// line. `source_offsets[text.len()]` is the source length, so the vector
    /// always has `text.len() + 1` entries and is non-decreasing.
    source_offsets: Vec<usize>,
}

impl NormalizedText {
    pub fn new(s: &str) -> Self {
        let mut text = String::with_capacity(s.len());
        let mut source_offsets = Vec::with_capacity(s.len() + 1);
        let mut blank_count = 0u32;

        let mut line_start = 0usize;
        while line_start < s.len() {
            let (line, newline_at, next_start) = match s[line_start..].find('\n') {
                Some(rel) => {
                    let line = &s[line_start..line_start + rel];
                    let newline_at = if line.ends_with('\r') {
                        line_start + rel - 1
                    } else {
                        line_start + rel
                    };
                    (line, newline_at, line_start + rel + 1)
                }
                // Unterminated last line: `str::lines` still yields it, and the
                // normalized form gets a synthetic terminator mapped to EOF.
                None => (&s[line_start..], s.len(), s.len()),
            };

            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                blank_count += 1;
                if blank_count <= 1 {
                    text.push('\n');
                    source_offsets.push(newline_at);
                }
            } else {
                blank_count = 0;
                for (rel, ch) in trimmed.char_indices() {
                    let at = line_start + rel;
                    if ch == '\t' {
                        for _ in 0..4 {
                            text.push(' ');
                            source_offsets.push(at);
                        }
                    } else {
                        text.push(ch);
                        for k in 0..ch.len_utf8() {
                            source_offsets.push(at + k);
                        }
                    }
                }
                text.push('\n');
                source_offsets.push(newline_at);
            }

            line_start = next_start;
        }

        source_offsets.push(s.len());
        debug_assert_eq!(source_offsets.len(), text.len() + 1);

        NormalizedText {
            text,
            source_offsets,
        }
    }

    /// Translate a span of the normalized text into the source byte range it
    /// covers.
    ///
    /// When the span ends on a normalized line terminator (every normalized
    /// line does, so any match of a normalized search string will), the
    /// returned range ends at the end of that source line's content, before
    /// its newline. Replacing the range with text that lacks a trailing
    /// newline therefore keeps the file's line structure intact, exactly as an
    /// exact-text match would. Source bytes that normalization dropped inside
    /// the span (trailing whitespace, collapsed blank lines) are included in
    /// the range; those dropped before the span are not.
    pub fn source_range(&self, norm_start: usize, norm_len: usize) -> (usize, usize) {
        let norm_end = norm_start.saturating_add(norm_len).min(self.text.len());
        let norm_start = norm_start.min(norm_end);
        let start = self.source_offsets[norm_start];
        let end = if norm_end > norm_start && self.text.as_bytes()[norm_end - 1] == b'\n' {
            self.source_offsets[norm_end - 1]
        } else {
            self.source_offsets[norm_end]
        };
        (start, end.max(start))
    }
}

pub fn normalize_whitespace(s: &str) -> String {
    NormalizedText::new(s).text
}

pub fn levenshtein_similarity(a: &str, b: &str) -> f64 {
    let a_len = a.chars().count();
    let b_len = b.chars().count();
    let max_len = a_len.max(b_len);
    if max_len == 0 {
        return 1.0;
    }
    let dist = levenshtein_distance(a, b);
    1.0 - (dist as f64 / max_len as f64)
}

fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let a_len = a_chars.len();
    let b_len = b_chars.len();

    if a_len == 0 {
        return b_len;
    }
    if b_len == 0 {
        return a_len;
    }

    let mut prev: Vec<usize> = (0..=b_len).collect();
    let mut curr = vec![0usize; b_len + 1];

    for i in 1..=a_len {
        curr[0] = i;
        for j in 1..=b_len {
            let cost = if a_chars[i - 1] == b_chars[j - 1] {
                0
            } else {
                1
            };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[b_len]
}
