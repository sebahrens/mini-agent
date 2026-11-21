use unicode_segmentation::UnicodeSegmentation;

/// Largest char boundary at or below `idx` (clamped to the string length).
pub fn floor_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Start of the Unicode scalar before `idx`. Picker queries are edited one
/// char at a time, so they pair this with `String::remove`.
pub fn prev_scalar_boundary(s: &str, idx: usize) -> usize {
    let mut i = floor_char_boundary(s, idx).saturating_sub(1);
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// End of the Unicode scalar at `idx`.
pub fn next_scalar_boundary(s: &str, idx: usize) -> usize {
    let len = s.len();
    let mut i = (idx + 1).min(len);
    while i < len && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Start of the extended grapheme cluster that ends at or before `idx`.
/// ZWJ emoji sequences, flags, skin-tone modifiers and base letters with
/// combining marks are one unit, so the cursor never stops inside one.
pub fn prev_char_boundary(s: &str, idx: usize) -> usize {
    let idx = floor_char_boundary(s, idx);
    match s[..idx].graphemes(true).next_back() {
        Some(cluster) => idx - cluster.len(),
        None => 0,
    }
}

/// End of the extended grapheme cluster that starts at or after `idx`.
pub fn next_char_boundary(s: &str, idx: usize) -> usize {
    let idx = floor_char_boundary(s, idx);
    match s[idx..].graphemes(true).next() {
        Some(cluster) => idx + cluster.len(),
        None => s.len(),
    }
}

pub fn cursor_to_line_col(buffer: &str, cursor: usize) -> (usize, usize) {
    let mut line = 0usize;
    let mut col = 0usize;
    for (i, ch) in buffer.char_indices() {
        if i >= cursor {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (line, col)
}

pub fn line_col_to_cursor(buffer: &str, target_line: usize, target_col: usize) -> usize {
    let mut line = 0usize;
    let mut col = 0usize;
    for (i, ch) in buffer.char_indices() {
        if line == target_line && col == target_col {
            return i;
        }
        if ch == '\n' {
            if line == target_line {
                return i;
            }
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    buffer.len()
}

pub fn count_lines(buffer: &str) -> usize {
    buffer.chars().filter(|&c| c == '\n').count() + 1
}

pub fn line_start(buffer: &str, cursor: usize) -> usize {
    let (line, _) = cursor_to_line_col(buffer, cursor);
    line_col_to_cursor(buffer, line, 0)
}

pub fn line_end(buffer: &str, cursor: usize) -> usize {
    let start = line_start(buffer, cursor);
    buffer[start..]
        .find('\n')
        .map(|pos| start + pos)
        .unwrap_or(buffer.len())
}
