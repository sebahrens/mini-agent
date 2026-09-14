pub(crate) mod file;
pub(crate) mod handlers;
pub(crate) mod list;
pub(crate) mod models;
pub(crate) mod rewind;

use std::io::Write;

use crossterm::ExecutableCommand;
use crossterm::cursor::MoveTo;
use crossterm::style::{Color, ResetColor, SetForegroundColor};
use crossterm::terminal::Clear;

use super::utils::resolve_color;

pub(crate) fn fuzzy_score(item: &str, query: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let item_l = item.to_lowercase();
    let query_l = query.to_lowercase();
    let is_boundary = |bytes: &[u8], pos: usize| -> bool {
        pos == 0
            || matches!(
                bytes.get(pos - 1),
                Some(b'-' | b'.' | b'/' | b'_' | b' ' | b':')
            )
    };

    if let Some(pos) = item_l.find(&query_l) {
        let mut score = 1000;
        if is_boundary(item_l.as_bytes(), pos) {
            score += 200;
        }
        if pos == 0 {
            score += 100;
        }
        score -= pos as i32;
        score -= (item_l.chars().count() / 4) as i32;
        return Some(score);
    }

    let chars: Vec<char> = item_l.chars().collect();
    let mut score = 0i32;
    let mut idx = 0usize;
    let mut last: Option<usize> = None;
    for qc in query_l.chars() {
        let mut pos = None;
        while idx < chars.len() {
            if chars[idx] == qc {
                pos = Some(idx);
                break;
            }
            idx += 1;
        }
        let pos = pos?;
        if last == Some(pos.wrapping_sub(1)) {
            score += 5;
        }
        if pos == 0 || matches!(chars.get(pos - 1), Some('-' | '.' | '/' | '_' | ' ' | ':')) {
            score += 3;
        }
        last = Some(pos);
        idx = pos + 1;
    }
    score -= (chars.len() / 20) as i32;
    Some(score)
}

/// Where a list overlay sits: rows `top_row..floor_row` show
/// `matches[start..end]`, scrolled to keep the selection in view.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PickerWindow {
    pub top_row: u16,
    pub start: usize,
    pub end: usize,
}

/// Lay out a list of `len` entries ending just above `floor_row` (the
/// separator over the input), leaving `reserved_above` rows for a header and
/// showing at most ten entries.
pub(crate) fn picker_window(
    floor_row: u16,
    reserved_above: u16,
    len: usize,
    selected: usize,
) -> PickerWindow {
    let max_items = floor_row.saturating_sub(reserved_above).min(10) as usize;
    let height = max_items.min(len);
    let start = selected
        .saturating_sub(height / 2)
        .min(len.saturating_sub(height));
    PickerWindow {
        top_row: floor_row.saturating_sub(height as u16),
        start,
        end: (start + height).min(len),
    }
}

/// Paint a one-line status message (e.g. "no matches") on the row just above
/// `floor_row`.
pub(crate) fn draw_picker_message(
    message: &str,
    monochrome: bool,
    floor_row: u16,
) -> std::io::Result<()> {
    let Some(row) = floor_row.checked_sub(1) else {
        return Ok(());
    };
    let mut stdout = std::io::stdout();
    stdout.execute(MoveTo(0, row))?;
    write!(
        stdout,
        "{}",
        Clear(crossterm::terminal::ClearType::CurrentLine)
    )?;
    write!(
        stdout,
        "{}{}{}",
        SetForegroundColor(resolve_color(Color::DarkGrey, monochrome)),
        message,
        ResetColor
    )?;
    stdout.flush()
}

pub(crate) fn draw_picker_list(
    matches: &[String],
    selected: usize,
    monochrome: bool,
    empty_message: Option<&str>,
    floor_row: u16,
    reserved_above: u16,
) -> std::io::Result<()> {
    if matches.is_empty() {
        return draw_picker_message(empty_message.unwrap_or("no matches"), monochrome, floor_row);
    }

    let (cols, _rows) = crossterm::terminal::size()?;
    let mut stdout = std::io::stdout();
    let window = picker_window(floor_row, reserved_above, matches.len(), selected);

    for (i, item) in matches
        .iter()
        .enumerate()
        .skip(window.start)
        .take(window.end - window.start)
    {
        let render_row = window.top_row + (i - window.start) as u16;
        stdout.execute(MoveTo(0, render_row))?;
        write!(
            stdout,
            "{}",
            Clear(crossterm::terminal::ClearType::CurrentLine)
        )?;

        let truncated = crate::ui::utils::display_prefix(item, cols.saturating_sub(3) as usize);

        if i == selected {
            write!(
                stdout,
                "{}",
                SetForegroundColor(resolve_color(Color::Green, monochrome))
            )?;
            write!(stdout, "▸ {}", truncated)?;
        } else {
            write!(
                stdout,
                "{}",
                SetForegroundColor(resolve_color(Color::DarkGrey, monochrome))
            )?;
            write!(stdout, "  {}", truncated)?;
        }
        write!(stdout, "{}", ResetColor)?;
    }
    stdout.flush()?;
    Ok(())
}
