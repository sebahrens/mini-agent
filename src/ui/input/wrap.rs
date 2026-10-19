//! Soft-wrap layout for the input box.
//!
//! Each logical line (split on `\n`) is broken into visual rows no wider than
//! the text width, preferring to break after whitespace. Rows are byte ranges
//! into the buffer, so the renderer (height, drawing, caret, clicks) and the
//! editor (Up/Down) share one layout.

use crate::ui::utils::{char_display_width, display_width};

/// One visual row: `buffer[start..end]`, never containing `\n`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisualRow {
    pub start: usize,
    pub end: usize,
}

/// Lay `buffer` out in rows at most `width` display columns wide (a width of
/// zero is treated as one). A row that exactly fills the width at the end of
/// the buffer is followed by an empty row, so a caret after it has a place
/// to sit instead of running off the edge.
pub fn wrap_rows(buffer: &str, width: usize) -> Vec<VisualRow> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut line_start = 0;
    for line in buffer.split('\n') {
        wrap_line(line, line_start, width, &mut rows);
        line_start += line.len() + 1;
    }
    if let Some(last) = rows.last()
        && last.end == buffer.len()
        && last.end > last.start
        && display_width(&buffer[last.start..last.end]) >= width
    {
        rows.push(VisualRow {
            start: buffer.len(),
            end: buffer.len(),
        });
    }
    rows
}

/// Length of the common byte prefix of `a` and `b`.
pub fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    const CHUNK: usize = 64;
    let limit = a.len().min(b.len());
    let mut at = 0;
    while at + CHUNK <= limit && a[at..at + CHUNK] == b[at..at + CHUNK] {
        at += CHUNK;
    }
    while at < limit && a[at] == b[at] {
        at += 1;
    }
    at
}

/// Length of the common byte suffix of `a` and `b`.
pub fn common_suffix_len(a: &[u8], b: &[u8]) -> usize {
    const CHUNK: usize = 64;
    let limit = a.len().min(b.len());
    let mut at = 0;
    while at + CHUNK <= limit
        && a[a.len() - at - CHUNK..a.len() - at] == b[b.len() - at - CHUNK..b.len() - at]
    {
        at += CHUNK;
    }
    while at < limit && a[a.len() - 1 - at] == b[b.len() - 1 - at] {
        at += 1;
    }
    at
}

/// [`wrap_rows`] of `new` computed from the rows of an earlier text that
/// differs from it only between a common byte prefix and a common byte
/// suffix (measured by the caller on the two texts; `old_rows` came from
/// `wrap_rows(old, width)` with `old.len() == old_len`). Only the logical
/// lines the edit touches are wrapped again; the rows before them are copied
/// and the rows after them shift by the length change. Returns the rows and
/// the number of bytes wrapped. The result equals `wrap_rows(new, width)`.
pub fn rewrap_edit(
    old_rows: &[VisualRow],
    old_len: usize,
    new: &str,
    width: usize,
    prefix: usize,
    suffix: usize,
) -> (Vec<VisualRow>, usize) {
    let width = width.max(1);
    let new_len = new.len();
    // The edit must not split a character: widen the changed span.
    let mut prefix = prefix.min(new_len).min(old_len);
    while !new.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let mut suffix = suffix.min(new_len - prefix).min(old_len - prefix);
    while !new.is_char_boundary(new_len - suffix) {
        suffix -= 1;
    }
    let tail_from = new_len - suffix;
    let line_start = new[..prefix].rfind('\n').map_or(0, |i| i + 1);
    let line_end_new = new[tail_from..]
        .find('\n')
        .map_or(new_len, |i| tail_from + i);
    // The same line end in the old text: the suffix is identical.
    let line_end_old = old_len - (new_len - line_end_new);
    let keep_head = old_rows.partition_point(|row| row.start < line_start);
    let keep_tail = old_rows.partition_point(|row| row.start <= line_end_old);
    let mut rows = Vec::with_capacity(old_rows.len() + 8);
    rows.extend_from_slice(&old_rows[..keep_head]);
    let mut offset = line_start;
    for line in new[line_start..line_end_new].split('\n') {
        wrap_line(line, offset, width, &mut rows);
        offset += line.len() + 1;
    }
    rows.extend(old_rows[keep_tail..].iter().map(|row| VisualRow {
        start: row.start + new_len - old_len,
        end: row.end + new_len - old_len,
    }));
    if line_end_new == new_len
        && let Some(last) = rows.last()
        && last.end == new_len
        && last.end > last.start
        && display_width(&new[last.start..last.end]) >= width
    {
        rows.push(VisualRow {
            start: new_len,
            end: new_len,
        });
    }
    (rows, line_end_new - line_start)
}

fn wrap_line(line: &str, offset: usize, width: usize, rows: &mut Vec<VisualRow>) {
    let mut row_start = 0;
    let mut row_width = 0;
    // Byte just after the last whitespace in the current row: a soft break.
    let mut break_after: Option<usize> = None;
    let mut indices = line.char_indices().peekable();
    while let Some((index, c)) = indices.next() {
        let cw = char_display_width(c);
        if row_width + cw > width && c.is_whitespace() {
            // Whitespace at the edge hangs off the row it ends, so the next
            // row starts with the following word instead of a lone space.
            let end = index + c.len_utf8();
            rows.push(VisualRow {
                start: offset + row_start,
                end: offset + end,
            });
            row_start = end;
            row_width = 0;
            break_after = None;
            continue;
        }
        if row_width + cw > width && index > row_start {
            let end = break_after.filter(|&b| b > row_start).unwrap_or(index);
            rows.push(VisualRow {
                start: offset + row_start,
                end: offset + end,
            });
            row_start = end;
            row_width = display_width(&line[row_start..index]);
            break_after = None;
        }
        row_width += cw;
        if c.is_whitespace() {
            break_after = Some(indices.peek().map_or(line.len(), |&(next, _)| next));
        }
    }
    rows.push(VisualRow {
        start: offset + row_start,
        end: offset + line.len(),
    });
}

/// The row holding byte offset `cursor`. At a soft break the caret belongs
/// to the start of the next row.
pub fn cursor_row(rows: &[VisualRow], cursor: usize) -> usize {
    rows.iter()
        .rposition(|row| row.start <= cursor)
        .unwrap_or(0)
        .min(rows.len().saturating_sub(1))
}

/// `(row, display column)` of byte offset `cursor`.
pub fn cursor_position(buffer: &str, rows: &[VisualRow], cursor: usize) -> (usize, usize) {
    let row_index = cursor_row(rows, cursor);
    let Some(row) = rows.get(row_index) else {
        return (0, 0);
    };
    let end = cursor.clamp(row.start, row.end);
    (row_index, display_width(&buffer[row.start..end]))
}

/// Byte offset in `row` at display column `column` (the nearest character
/// boundary at or before it, never past the row's end).
pub fn offset_at_column(buffer: &str, row: VisualRow, column: usize) -> usize {
    let mut width = 0;
    for (index, c) in buffer[row.start..row.end].char_indices() {
        let cw = char_display_width(c);
        if width + cw > column {
            return row.start + index;
        }
        width += cw;
    }
    row.end
}

/// Byte offset at display column `column` of row `index`, kept inside that
/// row: at a soft break the row's end is the next row's start, so the caret
/// stops one character earlier to stay on the row it moved to.
pub fn offset_in_row(buffer: &str, rows: &[VisualRow], index: usize, column: usize) -> usize {
    let row = rows[index];
    let offset = offset_at_column(buffer, row, column);
    let soft_break = rows
        .get(index + 1)
        .is_some_and(|next| next.start == row.end);
    if soft_break && offset == row.end && row.end > row.start {
        crate::ui::input::prev_char_boundary(buffer, row.end)
    } else {
        offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts<'a>(buffer: &'a str, rows: &[VisualRow]) -> Vec<&'a str> {
        rows.iter().map(|r| &buffer[r.start..r.end]).collect()
    }

    #[test]
    fn short_and_multiline_input_keeps_one_row_per_line() {
        let buffer = "ab\n\ncd";
        let rows = wrap_rows(buffer, 10);
        assert_eq!(texts(buffer, &rows), ["ab", "", "cd"]);
        assert_eq!(texts("", &wrap_rows("", 10)), [""]);
    }

    #[test]
    fn long_lines_break_after_whitespace_or_hard_when_there_is_none() {
        let buffer = "hello world again";
        assert_eq!(
            texts(buffer, &wrap_rows(buffer, 8)),
            ["hello ", "world ", "again"]
        );
        let buffer = "abcdefghij";
        assert_eq!(texts(buffer, &wrap_rows(buffer, 4)), ["abcd", "efgh", "ij"]);
        // A space at the edge hangs instead of starting the next row.
        let buffer = "abcd efgh";
        assert_eq!(texts(buffer, &wrap_rows(buffer, 4)), ["abcd ", "efgh", ""]);
        // Wide characters never straddle the edge.
        let buffer = "日本語テキスト";
        assert_eq!(
            texts(buffer, &wrap_rows(buffer, 5)),
            ["日本", "語テ", "キス", "ト"]
        );
    }

    #[test]
    fn a_full_last_row_gets_an_empty_row_for_the_caret() {
        let buffer = "abcd";
        let rows = wrap_rows(buffer, 4);
        assert_eq!(texts(buffer, &rows), ["abcd", ""]);
        assert_eq!(cursor_position(buffer, &rows, 4), (1, 0));
        assert_eq!(cursor_position(buffer, &rows, 3), (0, 3));
    }

    #[test]
    fn caret_positions_round_trip_through_rows_and_columns() {
        let buffer = "hello world\nx";
        let rows = wrap_rows(buffer, 8);
        assert_eq!(texts(buffer, &rows), ["hello ", "world", "x"]);
        assert_eq!(cursor_position(buffer, &rows, 6), (1, 0), "soft break");
        assert_eq!(cursor_position(buffer, &rows, 8), (1, 2));
        assert_eq!(cursor_position(buffer, &rows, 11), (1, 5));
        assert_eq!(cursor_position(buffer, &rows, 13), (2, 1));
        for cursor in [0, 3, 6, 8, 11, 12, 13] {
            let (row, column) = cursor_position(buffer, &rows, cursor);
            assert_eq!(offset_at_column(buffer, rows[row], column), cursor);
        }
        assert_eq!(offset_at_column(buffer, rows[1], 99), 11);
        let wide = "日本語";
        let rows = wrap_rows(wide, 10);
        assert_eq!(offset_at_column(wide, rows[0], 3), "日".len());
    }

    #[test]
    fn rewrapping_an_edit_equals_wrapping_the_whole_text() {
        // A small deterministic generator: no dependency, reproducible.
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move |bound: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % bound as u64) as usize
        };
        let alphabet = ["a", "bc", " ", "  ", "\n", "日", "é", "word", "xyzxyz"];
        for width in [1, 2, 5, 8, 20] {
            let mut text = String::new();
            for _ in 0..30 {
                text.push_str(alphabet[next(alphabet.len())]);
            }
            for _ in 0..300 {
                let old = text.clone();
                let old_rows = wrap_rows(&old, width);
                // Replace a random span with random pieces (insert, delete
                // or both), on character boundaries.
                let mut from = next(old.len() + 1);
                while !old.is_char_boundary(from) {
                    from -= 1;
                }
                let mut to = (from + next(6)).min(old.len());
                while !old.is_char_boundary(to) {
                    to += 1;
                }
                let mut insert = String::new();
                for _ in 0..next(3) {
                    insert.push_str(alphabet[next(alphabet.len())]);
                }
                text = format!("{}{insert}{}", &old[..from], &old[to..]);
                let prefix = common_prefix_len(old.as_bytes(), text.as_bytes());
                let suffix = common_suffix_len(old.as_bytes(), text.as_bytes());
                let (rows, _) = rewrap_edit(&old_rows, old.len(), &text, width, prefix, suffix);
                assert_eq!(
                    rows,
                    wrap_rows(&text, width),
                    "width {width}: {old:?} -> {text:?}"
                );
            }
        }
    }

    #[test]
    fn common_prefix_and_suffix_cover_chunked_and_short_inputs() {
        let a = "x".repeat(300);
        let mut b = a.clone();
        b.replace_range(150..151, "y");
        assert_eq!(common_prefix_len(a.as_bytes(), b.as_bytes()), 150);
        assert_eq!(common_suffix_len(a.as_bytes(), b.as_bytes()), 149);
        assert_eq!(common_prefix_len(a.as_bytes(), a.as_bytes()), 300);
        assert_eq!(common_suffix_len(b"", b"abc"), 0);
    }
}
