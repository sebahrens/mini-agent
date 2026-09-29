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
}
