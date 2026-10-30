pub(crate) mod bang;
pub(crate) mod config;
pub(crate) mod file;
pub(crate) mod handlers;
pub(crate) mod list;
pub(crate) mod models;
pub(crate) mod rewind;
pub(crate) mod sessions;
#[cfg(test)]
pub(crate) mod test_surface;

use std::io::Write;

use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::{Attribute, Color, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::Clear;

use super::utils::resolve_color;

fn picker_command<C: crossterm::Command + std::fmt::Display>(
    command: C,
) -> impl crossterm::Command + std::fmt::Display {
    #[cfg(test)]
    {
        test_surface::command(command)
    }
    #[cfg(not(test))]
    {
        command
    }
}

/// Geometry and writer of the test picker surface. Production pickers draw
/// only through `Renderer::draw_picker_with`, which owns both.
#[cfg(test)]
pub(crate) fn picker_size() -> std::io::Result<(u16, u16)> {
    test_surface::size()
}
#[cfg(test)]
pub(crate) fn picker_writer() -> impl std::io::Write {
    test_surface::writer()
}

/// Fuzzy-rank an already-lowercased `item_l` against a lowercase `query_l`:
/// a substring scores above a subsequence, and boundaries and early
/// positions score higher. Callers that filter many candidates lowercase
/// each haystack once (when the list is set) and the query once per
/// keystroke.
pub(crate) fn fuzzy_score_lower(item_l: &str, query_l: &str) -> Option<i32> {
    if query_l.is_empty() {
        return Some(0);
    }
    let is_boundary = |bytes: &[u8], pos: usize| -> bool {
        pos == 0
            || matches!(
                bytes.get(pos - 1),
                Some(b'-' | b'.' | b'/' | b'_' | b' ' | b':')
            )
    };

    if let Some(pos) = item_l.find(query_l) {
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

/// A picker entry made safe to write to the terminal: file names, gateway
/// model ids and history entries are untrusted, so every control character
/// (ESC, CR, LF, ...) and every bidi or zero-width format character that
/// could reorder or hide part of the name (see
/// [`is_deceptive_format_char`](crate::ui::events::is_deceptive_format_char))
/// is shown as U+FFFD instead of reaching the terminal. Only the drawn text
/// changes; accepting an entry inserts it unchanged.
pub(crate) fn display_safe(entry: &str) -> std::borrow::Cow<'_, str> {
    use crate::ui::events::needs_terminal_sanitizing;
    if entry.chars().any(needs_terminal_sanitizing) {
        entry
            .chars()
            .map(|c| {
                if needs_terminal_sanitizing(c) {
                    '\u{fffd}'
                } else {
                    c
                }
            })
            .collect::<String>()
            .into()
    } else {
        entry.into()
    }
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
/// `floor_row`. Commands are queued and flushed once.
pub(crate) fn draw_picker_message(
    mut out: &mut dyn Write,
    message: &str,
    monochrome: bool,
    floor_row: u16,
) -> std::io::Result<()> {
    let Some(row) = floor_row.checked_sub(1) else {
        return Ok(());
    };
    queue!(
        &mut out,
        picker_command(MoveTo(0, row)),
        picker_command(Clear(crossterm::terminal::ClearType::CurrentLine)),
    )?;
    write!(
        out,
        "{}{message}{ResetColor}",
        SetForegroundColor(resolve_color(Color::DarkGrey, monochrome))
    )?;
    out.flush()
}

/// Draw `matches` as a list overlay; see [`draw_picker_rows`].
#[cfg_attr(not(test), allow(dead_code))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_picker_list(
    out: &mut dyn Write,
    cols: u16,
    matches: &[String],
    selected: usize,
    monochrome: bool,
    empty_message: Option<&str>,
    floor_row: u16,
    reserved_above: u16,
) -> std::io::Result<()> {
    draw_picker_rows(
        out,
        cols,
        matches.len(),
        |index| std::borrow::Cow::Borrowed(matches[index].as_str()),
        selected,
        monochrome,
        None,
        empty_message,
        floor_row,
        reserved_above,
    )
}

/// Draw a list overlay of `len` entries ending above `floor_row`. Only the
/// visible window is formatted (`row(index)` is called for at most ten
/// entries); every row is queued and the writer is flushed once.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_picker_rows<'a>(
    out: &mut dyn Write,
    cols: u16,
    len: usize,
    row: impl Fn(usize) -> std::borrow::Cow<'a, str>,
    selected: usize,
    monochrome: bool,
    accent: Option<Color>,
    empty_message: Option<&str>,
    floor_row: u16,
    reserved_above: u16,
) -> std::io::Result<()> {
    draw_picker_described_rows(
        out,
        cols,
        len,
        |index| (row(index), None),
        selected,
        monochrome,
        accent,
        empty_message,
        floor_row,
        reserved_above,
    )
}

/// [`draw_picker_rows`] where a row may carry a dimmed detail column drawn
/// two spaces after its text. The detail only uses the width the text left
/// over and is cut, never wrapped, so each row stays one terminal line.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_picker_described_rows<'a>(
    mut out: &mut dyn Write,
    cols: u16,
    len: usize,
    row: impl Fn(usize) -> (std::borrow::Cow<'a, str>, Option<&'a str>),
    selected: usize,
    monochrome: bool,
    accent: Option<Color>,
    empty_message: Option<&str>,
    floor_row: u16,
    reserved_above: u16,
) -> std::io::Result<()> {
    if len == 0 {
        return draw_picker_message(
            out,
            empty_message.unwrap_or("no matches"),
            monochrome,
            floor_row,
        );
    }

    let window = picker_window(floor_row, reserved_above, len, selected);
    for i in window.start..window.end {
        let render_row = window.top_row + (i - window.start) as u16;
        queue!(
            &mut out,
            picker_command(MoveTo(0, render_row)),
            picker_command(Clear(crossterm::terminal::ClearType::CurrentLine)),
        )?;

        let (text, detail) = row(i);
        let item = display_safe(&text);
        let available = cols.saturating_sub(3) as usize;
        let truncated = crate::ui::utils::display_prefix(&item, available);
        let (color, marker) = if i == selected {
            (accent.unwrap_or(Color::Green), '▸')
        } else {
            (Color::DarkGrey, ' ')
        };
        write!(
            out,
            "{}{marker} {truncated}{ResetColor}",
            SetForegroundColor(resolve_color(color, monochrome)),
        )?;
        let remaining = available
            .saturating_sub(crate::ui::utils::display_width(truncated))
            .saturating_sub(2);
        if let Some(detail) = detail.filter(|detail| !detail.is_empty() && remaining > 0) {
            let detail = display_safe(detail);
            let detail = crate::ui::utils::display_prefix(&detail, remaining);
            write!(
                out,
                "  {}{}{detail}{}{ResetColor}",
                SetAttribute(Attribute::Dim),
                SetForegroundColor(resolve_color(Color::DarkGrey, monochrome)),
                SetAttribute(Attribute::NormalIntensity),
            )?;
        }
    }
    out.flush()
}
