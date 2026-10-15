/// Truncate `s` to at most `max` bytes on a UTF-8 char boundary, appending
/// `marker` so callers know content was capped. Plain `String::truncate` panics
/// mid-character (e.g. on CJK); this walks back to the nearest boundary.
pub(crate) fn truncate_cjk(s: &str, max: usize, marker: &str) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = s[..cut].to_string();
    out.push_str(marker);
    out
}

/// Number of lines [`str::lines`] yields for `s`, computed in one byte scan.
pub(crate) fn line_count(s: &str) -> usize {
    let newlines = s.bytes().filter(|&byte| byte == b'\n').count();
    newlines + usize::from(!s.is_empty() && !s.ends_with('\n'))
}

/// Byte offset where line `index` (0-based, as [`str::lines`] numbers them)
/// starts. `index` must not exceed the line count.
fn line_start(s: &str, index: usize) -> usize {
    if index == 0 {
        return 0;
    }
    s.bytes()
        .enumerate()
        .filter(|&(_, byte)| byte == b'\n')
        .nth(index - 1)
        .map_or(s.len(), |(offset, _)| offset + 1)
}

/// Append `count` lines of `s`, starting at byte `start` (a line start),
/// joined with `\n` exactly as `s[start..].lines().take(count)` would be.
/// Lines without a carriage return are copied as one contiguous slice.
fn push_lines(out: &mut String, s: &str, start: usize, count: usize) {
    if count == 0 {
        return;
    }
    let rest = &s[start..];
    let end = rest
        .bytes()
        .enumerate()
        .filter(|&(_, byte)| byte == b'\n')
        .nth(count - 1)
        .map_or(rest.len(), |(offset, _)| offset);
    let slice = &rest[..end];
    if !slice.contains('\r') {
        out.push_str(slice);
        return;
    }
    // `lines` strips a carriage return before each line feed; keep that
    // normalization for CRLF output instead of copying the raw bytes.
    for (index, line) in rest.lines().take(count).enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(line);
    }
}

/// Keep the first `max` lines of `s`. Returns `(head, total_lines)` where
/// `total_lines` is the original line count. If `total_lines <= max`, `head`
/// equals `s` (no truncation needed). Callers should append a tool-specific
/// recovery hint when truncation occurred.
#[cfg(test)]
pub(crate) fn head_lines(s: &str, max: usize) -> (String, usize) {
    let total = line_count(s);
    if total <= max {
        return (s.to_string(), total);
    }
    let mut head = String::new();
    push_lines(&mut head, s, 0, max);
    (head, total)
}

/// Keep the first `head` and last `tail` lines of `s` around `marker`, which
/// receives the total line count. Returns `None` when `s` has at most
/// `head + tail` lines. Equivalent to joining `s.lines()` ranges with `\n`,
/// but scans the text once per boundary and allocates one result string.
pub(crate) fn head_tail_lines(
    s: &str,
    head: usize,
    tail: usize,
    marker: impl FnOnce(usize) -> String,
) -> Option<String> {
    let total = line_count(s);
    if total <= head.saturating_add(tail) {
        return None;
    }
    let marker = marker(total);
    let tail_start = line_start(s, total - tail);
    let tail_bytes = s.len() - tail_start;
    let mut out =
        String::with_capacity(line_start(s, head).min(s.len()) + marker.len() + tail_bytes + 1);
    push_lines(&mut out, s, 0, head);
    out.push_str(&marker);
    if tail > 0 {
        out.push('\n');
        push_lines(&mut out, s, tail_start, tail);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(s: &str, head: usize, tail: usize) -> Option<String> {
        let total = s.lines().count();
        if total <= head + tail {
            return None;
        }
        let mut out = s.lines().take(head).collect::<Vec<_>>().join("\n");
        out.push_str(&format!("<{total}>"));
        if tail > 0 {
            out.push('\n');
            out.push_str(&s.lines().skip(total - tail).collect::<Vec<_>>().join("\n"));
        }
        Some(out)
    }

    #[test]
    fn line_count_matches_str_lines() {
        for text in [
            "",
            "a",
            "a\n",
            "\n",
            "\n\n",
            "a\nb",
            "a\r\nb\r\n",
            "a\n\nb\n",
        ] {
            assert_eq!(line_count(text), text.lines().count(), "{text:?}");
        }
    }

    #[test]
    fn head_tail_lines_matches_the_multi_pass_reference() {
        let texts = [
            "one\ntwo\nthree\nfour\nfive\nsix\nseven",
            "one\ntwo\nthree\nfour\nfive\nsix\nseven\n",
            "a\r\nb\r\nc\r\nd\r\ne\r\nf\r\n",
            "\n\n\n\n\n\n",
            "x\ny\r\nz\nw\r\nv\nu",
            "日本\n語\nテ\nキ\nス\nト",
        ];
        for text in texts {
            for head in 0..5 {
                for tail in 0..4 {
                    let actual = head_tail_lines(text, head, tail, |total| format!("<{total}>"));
                    assert_eq!(
                        actual,
                        reference(text, head, tail),
                        "{text:?} {head} {tail}"
                    );
                }
            }
        }
    }

    #[test]
    fn head_lines_keeps_short_text_and_counts_lines() {
        assert_eq!(head_lines("a\nb", 5), ("a\nb".to_string(), 2));
        assert_eq!(head_lines("a\nb\nc", 2), ("a\nb".to_string(), 3));
        assert_eq!(head_lines("a\r\nb\r\nc", 2), ("a\nb".to_string(), 3));
    }
}
