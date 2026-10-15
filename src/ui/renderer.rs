use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::LazyLock;

use compact_str::CompactString;
use crossterm::QueueableCommand;
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::style::{
    Attribute, Color, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate};
use regex::Regex;

use super::events::sanitize_output;
use super::feed::{BlockStyle, Feed, FeedLines, ForegroundPalette, style_from_color};
use super::statusline::StatusSpan;
use super::utils::{char_display_width, display_width, resolve_color};

static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://[^\x00-\x1f\x7f\s<>]+").expect("compile URL regex"));

#[cfg(test)]
thread_local! {
    /// URL regex scans on this thread, so paint tests can assert that rows
    /// without URLs never reach the regex.
    static URL_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Count one URL regex scan (test builds only).
fn note_url_scan() {
    #[cfg(test)]
    URL_SCANS.with(|scans| scans.set(scans.get() + 1));
}

#[cfg(test)]
pub(crate) fn url_scans_for_test() -> usize {
    URL_SCANS.with(std::cell::Cell::get)
}

/// Whether `text` might contain a URL for [`URL_RE`]: every match starts
/// with `http`, so text without it is painted without a regex scan.
fn may_contain_url(text: &str) -> bool {
    text.contains("http")
}

fn wrap_urls_osc8(text: &str) -> String {
    note_url_scan();
    let mut result = String::with_capacity(text.len() + 64);
    let mut last = 0;
    for m in URL_RE.find_iter(text) {
        result.push_str(&text[last..m.start()]);
        result.push_str("\x1b]8;;");
        result.push_str(m.as_str());
        result.push_str("\x1b\\");
        result.push_str(m.as_str());
        result.push_str("\x1b]8;;\x1b\\");
        last = m.end();
    }
    result.push_str(&text[last..]);
    result
}

/// Paint one laid-out chat row's text: the only escape sequences written
/// are the renderer's own reverse-video SGR (for a selection) and OSC 8
/// hyperlinks. Row text is sanitised again here, after the feed boundary,
/// so a row that somehow still carries a control sequence is painted inert
/// instead of reaching the terminal.
pub(crate) fn paint_row_text(
    out: &mut impl Write,
    chunk: &str,
    selected: Option<(usize, usize)>,
) -> io::Result<()> {
    match selected {
        Some((start, end)) => {
            write_painted_text(out, &chunk[..start])?;
            write!(out, "{}", SetAttribute(Attribute::Reverse))?;
            write_painted_text(out, &chunk[start..end])?;
            write!(out, "{}", SetAttribute(Attribute::NoReverse))?;
            write_painted_text(out, &chunk[end..])
        }
        None => write_painted_text(out, chunk),
    }
}

/// Write `text` exactly as [`paint_text`] would paint it. Plain text
/// (nothing to sanitize, no URL) is written straight from the row, with no
/// allocation and no regex scan.
fn write_painted_text(out: &mut impl Write, text: &str) -> io::Result<()> {
    if !may_contain_url(text) && !text.chars().any(super::events::needs_terminal_sanitizing) {
        return out.write_all(text.as_bytes());
    }
    out.write_all(paint_text(text).as_bytes())
}

/// Paint validated typed foreground spans without putting escape bytes in
/// copied text. Selection remains a byte-range overlay on the original row.
pub(crate) fn paint_styled_row(
    out: &mut impl Write,
    entry: &LineEntry,
    selected: Option<(usize, usize)>,
    monochrome: bool,
) -> io::Result<()> {
    let Some(spans) = entry.syntax_spans.as_ref().filter(|_| !monochrome) else {
        return paint_row_text(out, &entry.text, selected);
    };
    let mut previous = 0;
    for span in spans.iter() {
        if span.range.start < previous
            || span.range.start >= span.range.end
            || span.range.end > entry.text.len()
            || !entry.text.is_char_boundary(span.range.start)
            || !entry.text.is_char_boundary(span.range.end)
        {
            return paint_row_text(out, &entry.text, selected);
        }
        previous = span.range.end;
    }
    let mut paint = |range: std::ops::Range<usize>, color: Color| -> io::Result<()> {
        if range.is_empty() {
            return Ok(());
        }
        let selection = selected.and_then(|(start, end)| {
            let start = start.max(range.start);
            let end = end.min(range.end);
            (start < end).then(|| (start - range.start, end - range.start))
        });
        write!(out, "{}", SetForegroundColor(color))?;
        paint_row_text(out, &entry.text[range], selection)
    };
    // A syntax boundary must not cut a pre-existing OSC8 URL into unrelated fragments.
    // Treat complete inert URL text as one foreground run; selection still overlays it.
    let url_text: &str = if may_contain_url(&entry.text) {
        note_url_scan();
        &entry.text
    } else {
        ""
    };
    let mut urls = URL_RE.find_iter(url_text).peekable();
    let mut offset = 0;
    let mut span_index = 0;
    while offset < entry.text.len() {
        while spans
            .get(span_index)
            .is_some_and(|span| span.range.end <= offset)
        {
            span_index += 1;
        }
        while urls.peek().is_some_and(|url| url.end() <= offset) {
            urls.next();
        }
        let (mut end, mut color) = match spans.get(span_index) {
            Some(span) if span.range.start <= offset => (span.range.end, span.color),
            Some(span) => (span.range.start, entry.color),
            None => (entry.text.len(), entry.color),
        };
        if let Some(url) = urls.peek() {
            if url.start() <= offset {
                end = url.end();
                color = entry.color;
            } else {
                end = end.min(url.start());
            }
        }
        paint(offset..end, color)?;
        offset = end;
    }
    Ok(())
}

fn paint_text(text: &str) -> String {
    if text.chars().any(super::events::needs_terminal_sanitizing) {
        wrap_urls_osc8(&sanitize_output(text))
    } else {
        wrap_urls_osc8(text)
    }
}

#[derive(Clone, Debug)]
pub struct LineEntry {
    pub text: CompactString,
    pub color: Color,
    /// Ephemeral foreground spans; text stays plain for widths and copying.
    pub(crate) syntax_spans: Option<std::sync::Arc<[super::highlight::TokenSpan]>>,
}

pub struct PermissionPrompt {
    pub tool: CompactString,
    pub options: CompactString,
    /// Whether `e` expanded the prompt to show as much of the request as
    /// the screen allows instead of the compact summary.
    pub expanded: bool,
}

pub struct ChainPrompt {
    pub question: CompactString,
}

/// Everything that affects what the chat viewport paints. Compared between
/// frames to decide whether `render_viewport` can skip drawing.
#[derive(Clone, PartialEq)]
struct ChatSnapshot {
    feed_generation: u64,
    width: usize,
    visible_rows: usize,
    scroll_offset: usize,
    selection_active: bool,
    selection_start: Option<SelectionPoint>,
    selection_end: Option<SelectionPoint>,
    chat_bg: Option<Color>,
    monochrome: bool,
}

/// Which prompt mode `draw_bottom` paints in the input area.
#[derive(Clone, PartialEq)]
pub(crate) enum PromptSnapshot {
    Input,
    Permission {
        tool: CompactString,
        options: CompactString,
        expanded: bool,
    },
    Chain {
        question: CompactString,
        but_mode: bool,
    },
}

/// Everything `draw_bottom` paints, compared between frames to decide how much
/// of the bottom region (input area + statusline) needs repainting.
#[derive(Clone, PartialEq)]
pub(crate) struct BottomSnapshot {
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    pub(crate) statusline_height: usize,
    pub(crate) input_hash: u64,
    pub(crate) cursor_pos: usize,
    pub(crate) is_running: bool,
    pub(crate) spinner_frame: u8,
    pub(crate) input_vscroll_offset: usize,
    pub(crate) prompt: PromptSnapshot,
    pub(crate) statusline_key: u64,
    pub(crate) scroll_indicator: bool,
    pub(crate) monochrome: bool,
    pub(crate) input_bg: Option<Color>,
    pub(crate) status_bg: Option<Color>,
    /// Text of the transient notice shown on the status line, if any.
    pub(crate) notice: Option<CompactString>,
}

/// How much of the bottom region a `draw_bottom` call must repaint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BottomRedrawPlan {
    /// Nothing changed since the last frame; draw nothing.
    Skip,
    /// Only the statusline content changed; redraw just the statusline rows.
    StatuslineOnly,
    /// Input area (or the geometry it sits in) changed; full bottom redraw.
    Full,
}

/// One statusline row as painted, keyed by everything that shapes it.
struct FittedStatusRow {
    key: u64,
    bytes: Vec<u8>,
}

struct StatuslineCache {
    key: u64,
    lines: Arc<Vec<Vec<StatusSpan>>>,
}

/// Display width the input text wraps at: the terminal width less the
/// two-column prompt (`> ` or a spinner frame), never below one column.
fn input_text_width(cols: u16) -> usize {
    (cols as usize).saturating_sub(display_width("> ")).max(1)
}

const SPINNER: &[&str] = &["⠋ ", "⠙ ", "⠹ ", "⠸ ", "⠼ ", "⠴ ", "⠦ ", "⠧ ", "⠇ ", "⠏ "];

/// Clamp a chat scroll offset to the scrollable range `0..=total - visible`.
///
/// The offset is set while scrolling but the viewport can grow afterwards
/// (input box shrinks, terminal resized), which would otherwise leave
/// `total - offset - visible` underflowing.
pub(crate) fn clamp_scroll_offset(offset: usize, total: usize, visible: usize) -> usize {
    offset.min(total.saturating_sub(visible))
}

/// Position of a scrolled viewport as a percentage: 0 at the top of the
/// scrollback, 100 at the bottom. Saturating on every operand so a stale
/// offset or a viewport taller than the content never panics.
pub(crate) fn scroll_percent(offset: usize, total: usize, visible: usize) -> usize {
    let range = total.saturating_sub(visible);
    if range == 0 {
        return 0;
    }
    let offset = offset.min(range);
    ((range - offset).saturating_mul(100) / range).min(100)
}

/// One end of a transcript selection: a laid-out chat line and a display
/// column within that line's text (the chat margin excluded).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SelectionPoint {
    pub line: usize,
    pub col: usize,
}

impl SelectionPoint {
    pub fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

/// Byte index of the character covering display column `col` in `text`, or
/// `text.len()` when the column lies past the end of the text.
fn byte_at_display_col(text: &str, col: usize) -> usize {
    let mut width = 0usize;
    for (index, ch) in text.char_indices() {
        let char_width = char_display_width(ch);
        if col < width + char_width.max(1) {
            return index;
        }
        width += char_width;
    }
    text.len()
}

/// Byte range of `text` (laid-out line `line`) covered by the selection
/// between `a` and `b`, in either order. Both ends are inclusive of the
/// character under the pointer; lines strictly inside the selection are
/// covered whole. `None` when the line is outside the selection.
pub(crate) fn selection_byte_range(
    text: &str,
    line: usize,
    a: SelectionPoint,
    b: SelectionPoint,
) -> Option<(usize, usize)> {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    if line < lo.line || line > hi.line {
        return None;
    }
    let start = if line == lo.line {
        byte_at_display_col(text, lo.col)
    } else {
        0
    };
    let end = if line == hi.line {
        let at = byte_at_display_col(text, hi.col);
        text[at..]
            .chars()
            .next()
            .map_or(at, |ch| at + ch.len_utf8())
    } else {
        text.len()
    };
    Some((start, end.max(start)))
}

/// Longest gap between two presses that still counts as a double-click.
pub(crate) const DOUBLE_CLICK_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);

/// Where and when the left button last went down in the transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClickAt {
    pub at: std::time::Instant,
    pub row: u16,
    pub col: u16,
}

/// Whether a press at `(row, col)` at `now` completes a double-click with
/// the `previous` press: same row, at most one column apart (a hand
/// wobble), within [`DOUBLE_CLICK_WINDOW`].
pub(crate) fn is_double_click(
    previous: Option<ClickAt>,
    now: std::time::Instant,
    row: u16,
    col: u16,
) -> bool {
    previous.is_some_and(|prev| {
        prev.row == row
            && prev.col.abs_diff(col) <= 1
            && now.saturating_duration_since(prev.at) <= DOUBLE_CLICK_WINDOW
    })
}

/// Characters a double-click word extends over besides letters and digits,
/// so a path, identifier or `file.rs:12` location is taken whole.
fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | '~' | ':')
}

/// The word under display column `col` of `text`, as inclusive display
/// columns `(start, end)`. Trailing `.` and `:` (sentence punctuation) are
/// not part of it. `None` when the column holds no word character.
pub(crate) fn word_cols_at(text: &str, col: usize) -> Option<(usize, usize)> {
    let mut cells: Vec<(usize, char)> = Vec::new();
    let mut width = 0usize;
    for ch in text.chars() {
        cells.push((width, ch));
        width += char_display_width(ch);
    }
    let hit = cells.iter().rposition(|&(start, _)| start <= col)?;
    let (hit_start, hit_char) = cells[hit];
    if !is_word_char(hit_char) || col >= hit_start + char_display_width(hit_char).max(1) {
        return None;
    }
    let mut first = hit;
    while first > 0 && is_word_char(cells[first - 1].1) {
        first -= 1;
    }
    let mut last = hit;
    while last + 1 < cells.len() && is_word_char(cells[last + 1].1) {
        last += 1;
    }
    while last > hit && matches!(cells[last].1, '.' | ':') {
        last -= 1;
    }
    Some((cells[first].0, cells[last].0))
}

/// Marker painted at the top-right of the chat viewport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HistoryIndicator {
    /// The view is scrolled back: position as a percentage.
    Scrolled(String),
    /// Following the output, but earlier transcript exists above the view.
    Above(String),
}

/// Which viewport marker to show: the scroll position while scrolled back,
/// otherwise a dim count of the lines above the view whenever the transcript
/// is taller than the viewport, so there is always a cue that history can be
/// scrolled to (terminal scrollback is unavailable on the alternate screen).
pub(crate) fn history_indicator(
    offset: usize,
    total: usize,
    visible: usize,
) -> Option<HistoryIndicator> {
    if offset > 0 {
        let pct = scroll_percent(offset, total, visible);
        return Some(HistoryIndicator::Scrolled(format!(" SCROLL {pct}% ")));
    }
    let above = total.saturating_sub(visible);
    (above > 0).then(|| HistoryIndicator::Above(format!(" ↑ {above} ")))
}

/// First terminal row of the input box for a terminal with `rows` rows,
/// `reserve` rows kept for the status line, and an input `visible_line_count`
/// rows tall. Saturating: terminals shorter than the reserved area yield row 1
/// instead of a `u16` underflow.
pub(crate) fn input_top_row(rows: u16, reserve: u16, visible_line_count: usize) -> u16 {
    let input_rows = u16::try_from(visible_line_count).unwrap_or(u16::MAX);
    rows.saturating_sub(reserve)
        .saturating_sub(input_rows)
        .saturating_add(1)
}

/// Most rows a permission/chain prompt may take from a bottom area with
/// `available_rows` rows above the status line: enough to read a wrapped
/// path or the start of a script, never so many that the transcript
/// disappears.
pub(crate) fn prompt_max_rows(available_rows: usize) -> usize {
    (available_rows * 2 / 5).clamp(2, 12)
}

/// Most rows an expanded permission prompt may take: everything above the
/// status line except one transcript row.
pub(crate) fn expanded_prompt_max_rows(available_rows: usize) -> usize {
    available_rows.saturating_sub(1).max(2)
}

/// Options hint of a permission prompt whose request was shortened.
pub(crate) const PROMPT_EXPAND_HINT: &str = "(e) show full request";
/// Options hint of an expanded permission prompt.
pub(crate) const PROMPT_COLLAPSE_HINT: &str = "(e) collapse";

/// Lay out a permission prompt in a bottom area of `available_rows` rows.
/// Collapsed, it is the compact [`prompt_block_rows`] layout, and when that
/// had to shorten the request the options gain an `(e)` hint to expand it.
/// Expanded, the request gets every row but one of the area, so a long
/// path wraps in full and a script shows as many lines as fit.
pub(crate) fn permission_prompt_rows(
    header: &str,
    options: &str,
    width: usize,
    available_rows: usize,
    expanded: bool,
) -> Vec<String> {
    if expanded {
        let options = format!("{options}  {PROMPT_COLLAPSE_HINT}");
        return prompt_block_rows(
            header,
            &options,
            width,
            expanded_prompt_max_rows(available_rows),
        );
    }
    let max_rows = prompt_max_rows(available_rows);
    let rows = prompt_block_rows(header, options, width, max_rows);
    if !prompt_header_shortened(header, options, width, &rows) {
        return rows;
    }
    let options = format!("{options}  {PROMPT_EXPAND_HINT}");
    prompt_block_rows(header, &options, width, max_rows)
}

/// Whether the header rows of a [`prompt_block_rows`] layout differ from
/// the full request wrapped to `width` (lines hidden or a path elided).
fn prompt_header_shortened(header: &str, options: &str, width: usize, rows: &[String]) -> bool {
    use crate::ui::utils::wrap_to_width;
    let width = width.max(1);
    let header = crate::ui::events::sanitize_for_review(header);
    let options = crate::ui::events::sanitize_output(options).replace('\n', " ");
    let option_rows = wrap_to_width(&options, width).len().min(rows.len());
    let full: Vec<String> = header
        .split('\n')
        .flat_map(|line| wrap_to_width(line, width))
        .collect();
    rows[..rows.len() - option_rows] != full[..]
}

/// Lay out a permission (or chain) prompt as terminal rows: the sanitized
/// `header` hard-wrapped to `width`, then the `options`, in at most
/// `max_rows` rows. Nothing is ever written raw: a multi-line header (a
/// heredoc, a `node -e` script) shows its first lines and a note counting
/// the rest, and a single over-long line (a deep path) is elided in the
/// middle so its file name stays visible. The full request is also in the
/// transcript, which can be scrolled while the prompt waits.
pub(crate) fn prompt_block_rows(
    header: &str,
    options: &str,
    width: usize,
    max_rows: usize,
) -> Vec<String> {
    use crate::ui::utils::{compact_multiline, middle_elide, wrap_to_width};

    let width = width.max(1);
    let max_rows = max_rows.max(2);
    let header = crate::ui::events::sanitize_for_review(header);
    let options = crate::ui::events::sanitize_output(options).replace('\n', " ");
    let mut option_rows = wrap_to_width(&options, width);
    option_rows.truncate(max_rows - 1);
    let budget = max_rows - option_rows.len();
    let lines: Vec<&str> = header.split('\n').collect();

    let fit = |line: &str, rows: usize| {
        let mut wrapped = wrap_to_width(line, width);
        if wrapped.len() > rows {
            wrapped = wrap_to_width(&middle_elide(line, rows * width), width);
            wrapped.truncate(rows);
        }
        wrapped
    };

    let mut rows = Vec::with_capacity(max_rows);
    if lines.len() == 1 {
        rows = fit(&header, budget);
    } else if budget == 1 {
        rows.push(middle_elide(
            &compact_multiline(&header, header.len()),
            width,
        ));
    } else {
        let content_budget = budget - 1;
        let mut shown = 0usize;
        for line in &lines {
            let room = content_budget - rows.len();
            if room == 0 {
                break;
            }
            let wrapped = wrap_to_width(line, width);
            let complete = wrapped.len() <= room;
            rows.extend(if complete { wrapped } else { fit(line, room) });
            if !complete {
                break;
            }
            shown += 1;
        }
        let hidden = lines.len() - shown;
        if hidden > 0 {
            let note = format!(
                "… {hidden} more line(s), {} chars in total: scroll up (PgUp / wheel) to review the full request",
                header.chars().count()
            );
            rows.push(crate::ui::utils::display_prefix(&note, width).to_string());
        }
    }
    rows.extend(option_rows);
    rows
}

#[cfg(test)]
type NativeSizeQuery = fn() -> io::Result<(u16, u16)>;

/// Test-only output and paint counters shared by a renderer and the owned
/// writers it hands out, so frame-cost assertions can measure writes, bytes,
/// flushes and paints without parsing escape sequences.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct OutputCounters {
    writes: std::sync::atomic::AtomicUsize,
    bytes: std::sync::atomic::AtomicUsize,
    flushes: std::sync::atomic::AtomicUsize,
    viewport_paints: std::sync::atomic::AtomicUsize,
    viewport_rows: std::sync::atomic::AtomicUsize,
    bottom_full_paints: std::sync::atomic::AtomicUsize,
    bottom_statusline_paints: std::sync::atomic::AtomicUsize,
    statusline_fits: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl OutputCounters {
    fn bump(counter: &std::sync::atomic::AtomicUsize, by: usize) {
        counter.fetch_add(by, std::sync::atomic::Ordering::Relaxed);
    }
}

/// A snapshot of [`OutputCounters`].
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FrameStats {
    /// `write` calls that reached the owned output.
    pub(crate) writes: usize,
    /// Bytes that reached the owned output.
    pub(crate) bytes: usize,
    /// `flush` calls on the owned output.
    pub(crate) flushes: usize,
    /// `render_viewport` calls that painted (not skipped).
    pub(crate) viewport_paints: usize,
    /// Chat rows painted by those calls.
    pub(crate) viewport_rows: usize,
    /// Full bottom-region draws.
    pub(crate) bottom_full_paints: usize,
    /// Statusline-only bottom draws.
    pub(crate) bottom_statusline_paints: usize,
    /// Statusline rows fitted and painted (not served from the row cache).
    pub(crate) statusline_fits: usize,
}

#[cfg(test)]
impl FrameStats {
    /// Counters accumulated since `earlier`.
    pub(crate) fn since(self, earlier: FrameStats) -> FrameStats {
        FrameStats {
            writes: self.writes - earlier.writes,
            bytes: self.bytes - earlier.bytes,
            flushes: self.flushes - earlier.flushes,
            viewport_paints: self.viewport_paints - earlier.viewport_paints,
            viewport_rows: self.viewport_rows - earlier.viewport_rows,
            bottom_full_paints: self.bottom_full_paints - earlier.bottom_full_paints,
            bottom_statusline_paints: self.bottom_statusline_paints
                - earlier.bottom_statusline_paints,
            statusline_fits: self.statusline_fits - earlier.statusline_fits,
        }
    }
}

/// Elapsed-time trace for one painted region, emitted on the `ui::frame`
/// target (`RUST_LOG=ui::frame=trace`). The clock is read only when the
/// target is enabled, so disabled tracing costs one callsite check.
struct FrameTrace {
    region: &'static str,
    started: Option<std::time::Instant>,
}

/// Escape bytes that open and close one terminal frame (DEC 2026
/// synchronized update); terminals that do not know the mode ignore them.
static SYNC_BEGIN: LazyLock<String> = LazyLock::new(|| ansi(BeginSynchronizedUpdate));
static SYNC_END: LazyLock<String> = LazyLock::new(|| ansi(EndSynchronizedUpdate));

fn ansi(command: impl crossterm::Command) -> String {
    let mut text = String::new();
    command
        .write_ansi(&mut text)
        .expect("writing an escape sequence to a String cannot fail");
    text
}

/// Writer into the renderer's frame buffer. Paint paths queue their escape
/// sequences and text here; the frame is written and flushed once, at the
/// outermost frame boundary (see `Renderer::frame`). Its `flush` is a no-op.
struct FrameOut<'a>(&'a std::cell::RefCell<Vec<u8>>);

impl Write for FrameOut<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A writer into `$renderer`'s frame buffer, borrowing only that field so
/// paint code can keep updating other renderer state while it writes.
macro_rules! frame_out {
    ($renderer:expr) => {{
        debug_assert!($renderer.frame_depth > 0, "painted outside a frame");
        FrameOut(&$renderer.frame)
    }};
}

impl FrameTrace {
    fn start(region: &'static str) -> Self {
        let started = tracing::enabled!(target: "ui::frame", tracing::Level::TRACE)
            .then(std::time::Instant::now);
        Self { region, started }
    }

    fn finish(self, bytes: usize) {
        if let Some(started) = self.started {
            tracing::trace!(
                target: "ui::frame",
                region = self.region,
                bytes,
                elapsed_us = started.elapsed().as_micros() as u64,
                "frame painted"
            );
        }
    }
}

pub struct Renderer {
    /// An owned terminal-byte capture for App tests, never process stdout.
    #[cfg(test)]
    test_output: Option<Arc<std::sync::Mutex<Vec<u8>>>>,
    #[cfg(test)]
    test_fail_next_write: std::cell::Cell<bool>,
    #[cfg(test)]
    test_native_size_query: Option<NativeSizeQuery>,
    #[cfg(test)]
    native_size_queries: std::cell::Cell<usize>,
    /// A captured renderer that still takes its geometry from the native
    /// query (`test_native_size_query`), to count queries per frame.
    #[cfg(test)]
    test_native_geometry: bool,
    /// Terminal geometry from the last native query (`terminal_size`).
    cached_size: std::cell::Cell<Option<(u16, u16)>>,
    #[cfg(test)]
    test_counters: Arc<OutputCounters>,
    /// The frame being painted: every paint path appends here and the
    /// outermost `frame` call writes it with one `write_all` + flush.
    frame: std::cell::RefCell<Vec<u8>>,
    /// Nesting depth of `frame` calls; the frame is flushed at depth zero.
    frame_depth: u32,
    /// A line was written inside an open frame; the frame's close paints
    /// the chat once for all of them.
    deferred_line_paint: bool,
    spinner_frame: u8,
    feed: Feed,
    seen_feed_retention_generation: u64,
    scroll_offset: usize,
    input_vscroll_offset: usize,
    input_max_vscroll: usize,
    last_input_cursor: usize,
    // Geometry of the last-rendered input area, used to map a mouse click to a
    // cursor position inside the input buffer.
    input_base_row: u16,
    input_prompt_width: usize,
    input_first_visible: usize,
    input_visible_line_count: usize,
    monochrome: bool,
    palette: ForegroundPalette,
    terminal_background: Option<Color>,
    chat_bg: Option<Color>,
    input_bg: Option<Color>,
    status_bg: Option<Color>,
    pub selection_active: bool,
    pub selection_start: Option<SelectionPoint>,
    pub selection_end: Option<SelectionPoint>,
    /// Set once the pointer moved while the button was held. A plain click
    /// (press + release without movement) never copies.
    pub selection_dragged: bool,
    /// The selection is a double-clicked word: releasing the button copies
    /// it as selected instead of moving its end to the pointer.
    pub selection_word: bool,
    /// The last transcript press, for double-click detection.
    last_click: Option<ClickAt>,
    prev_input_height: usize,
    /// Number of statusline rows (1-3), fixed by the statusline config at startup.
    statusline_height: usize,
    /// Left padding (columns) for the chat buffer area only. Input and status
    /// rows are unaffected.
    chat_margin: u16,
    pub permission_prompt: Option<PermissionPrompt>,
    pub chain_prompt: Option<ChainPrompt>,
    pub chain_but_mode: bool,
    /// Dirty-region tracking: explicit invalidation flags plus snapshots of
    /// the state recorded after the last successful draw of each region.
    chat_dirty: bool,
    last_chat_snapshot: Option<ChatSnapshot>,
    bottom_dirty: bool,
    last_bottom_snapshot: Option<BottomSnapshot>,
    statusline_cache: Option<StatuslineCache>,
    /// Painted statusline rows by row index (see `draw_statusline`).
    statusline_rows: std::cell::RefCell<Vec<Option<FittedStatusRow>>>,
    #[cfg(test)]
    statusline_builds: usize,
    /// Screen position of the input caret after the last full bottom draw;
    /// `None` when the caret is hidden (permission/chain prompts).
    bottom_cursor: Option<(u16, u16)>,
    /// Transient status-line message (e.g. a copy result) that must not
    /// become a permanent transcript entry.
    notice: Option<Notice>,
    /// Report agent activity in the terminal title (config `terminal_title`).
    title_status: bool,
    /// Activity last written to the title, so unchanged frames write nothing.
    title_activity: Option<crate::ui::terminal::AgentActivity>,
    /// Opt-in notification and prompt-mark sequences (config
    /// `terminal_notify`, `terminal_prompt_marks`).
    activity_signals: crate::ui::terminal::ActivitySignals,
    /// Activity of the last frame, for the transitions those signals report.
    last_activity: Option<crate::ui::terminal::AgentActivity>,
}

/// A status-line message that disappears on its own.
struct Notice {
    text: CompactString,
    color: Color,
    until: std::time::Instant,
}

#[cfg(test)]
struct TestOutput {
    sink: TestSink,
    counters: Arc<OutputCounters>,
}

#[cfg(test)]
enum TestSink {
    Capture(Arc<std::sync::Mutex<Vec<u8>>>),
    Fail,
    Stdout(io::Stdout),
}

#[cfg(test)]
impl Write for TestOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.sink.write(bytes)?;
        OutputCounters::bump(&self.counters.writes, 1);
        OutputCounters::bump(&self.counters.bytes, written);
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.sink.flush()?;
        OutputCounters::bump(&self.counters.flushes, 1);
        Ok(())
    }
}

#[cfg(test)]
impl Write for TestSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Fail => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "owned test output failure",
            )),
            Self::Capture(capture) => capture
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .write(bytes),
            Self::Stdout(stdout) => stdout.write(bytes),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Fail => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "owned test output failure",
            )),
            Self::Capture(capture) => capture
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .flush(),
            Self::Stdout(stdout) => stdout.flush(),
        }
    }
}

/// How long a transient notice stays on the status line.
pub(crate) const NOTICE_DURATION: std::time::Duration = std::time::Duration::from_secs(3);

impl Renderer {
    pub fn new() -> io::Result<Self> {
        Ok(Renderer {
            #[cfg(test)]
            test_output: None,
            #[cfg(test)]
            test_fail_next_write: std::cell::Cell::new(false),
            #[cfg(test)]
            test_native_size_query: None,
            #[cfg(test)]
            native_size_queries: std::cell::Cell::new(0),
            #[cfg(test)]
            test_native_geometry: false,
            cached_size: std::cell::Cell::new(None),
            #[cfg(test)]
            test_counters: Arc::new(OutputCounters::default()),
            frame: std::cell::RefCell::new(Vec::with_capacity(16 * 1024)),
            frame_depth: 0,
            deferred_line_paint: false,
            spinner_frame: 0,
            feed: Feed::new(),
            seen_feed_retention_generation: 0,
            scroll_offset: 0,
            input_vscroll_offset: 0,
            input_max_vscroll: 0,
            last_input_cursor: 0,
            input_base_row: 0,
            input_prompt_width: 0,
            input_first_visible: 0,
            input_visible_line_count: 0,
            monochrome: false,
            palette: ForegroundPalette::default(),
            terminal_background: None,
            chat_bg: None,
            input_bg: None,
            status_bg: None,
            selection_active: false,
            selection_start: None,
            selection_end: None,
            selection_dragged: false,
            selection_word: false,
            last_click: None,
            prev_input_height: 0,
            statusline_height: 1,
            chat_margin: 0,
            permission_prompt: None,
            chain_prompt: None,
            chain_but_mode: false,
            chat_dirty: true,
            last_chat_snapshot: None,
            bottom_dirty: true,
            last_bottom_snapshot: None,
            statusline_cache: None,
            statusline_rows: std::cell::RefCell::new(Vec::new()),
            #[cfg(test)]
            statusline_builds: 0,
            bottom_cursor: None,
            notice: None,
            title_status: false,
            title_activity: None,
            activity_signals: crate::ui::terminal::ActivitySignals::default(),
            last_activity: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_app_test() -> io::Result<Self> {
        let mut renderer = Self::new()?;
        renderer.test_output = Some(Arc::new(std::sync::Mutex::new(Vec::new())));
        Ok(renderer)
    }

    /// A captured renderer whose geometry comes from `query`, as if it
    /// were the native terminal.
    #[cfg(test)]
    pub(crate) fn for_native_geometry_test(query: NativeSizeQuery) -> io::Result<Self> {
        let mut renderer = Self::for_app_test()?;
        renderer.test_native_geometry = true;
        renderer.test_native_size_query = Some(query);
        Ok(renderer)
    }

    #[cfg(test)]
    pub(crate) fn captured_output(&self) -> String {
        let bytes = self
            .test_output
            .as_ref()
            .expect("test capture configured")
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[cfg(test)]
    pub(crate) fn capture_bytes(&self) -> Arc<std::sync::Mutex<Vec<u8>>> {
        self.test_output
            .as_ref()
            .expect("test capture configured")
            .clone()
    }

    /// Write one finished frame to the terminal: a single `write_all` and
    /// flush on the locked stdout handle.
    #[cfg(not(test))]
    fn write_frame(&self, bytes: &[u8]) -> io::Result<()> {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        out.write_all(bytes)?;
        out.flush()
    }

    #[cfg(test)]
    fn write_frame(&self, bytes: &[u8]) -> io::Result<()> {
        let mut out = self.writer();
        out.write_all(bytes)?;
        out.flush()
    }

    /// Run `paint` inside one terminal frame. Nested calls join the
    /// enclosing frame; the outermost call wraps everything painted in a
    /// synchronized update and writes it with one `write_all` + flush. A
    /// frame that painted nothing writes nothing. When that write fails, the
    /// regions are marked dirty so the next frame repaints them in full.
    pub(crate) fn frame<R>(
        &mut self,
        paint: impl FnOnce(&mut Self) -> io::Result<R>,
    ) -> io::Result<R> {
        self.begin_frame();
        let result = paint(self);
        let flushed = self.end_frame();
        let value = result?;
        flushed?;
        Ok(value)
    }

    /// Open a frame (see [`Renderer::frame`]) that a later
    /// [`Renderer::end_frame`] closes. Lines written while it is open are
    /// painted once, when the outermost frame closes, instead of one chat
    /// repaint per line. Callers that cannot wrap their work in a closure
    /// use this pair; every `begin_frame` must be matched.
    pub(crate) fn begin_frame(&mut self) {
        if self.frame_depth == 0 {
            let mut frame = self.frame.borrow_mut();
            frame.clear();
            frame.extend_from_slice(SYNC_BEGIN.as_bytes());
        }
        self.frame_depth += 1;
    }

    /// Close a frame opened by [`Renderer::begin_frame`]; the outermost
    /// close paints lines written meanwhile and writes the frame.
    pub(crate) fn end_frame(&mut self) -> io::Result<()> {
        debug_assert!(self.frame_depth > 0, "end_frame without begin_frame");
        if self.frame_depth > 1 {
            self.frame_depth -= 1;
            return Ok(());
        }
        let painted = if std::mem::take(&mut self.deferred_line_paint) {
            self.render_viewport_in_frame()
        } else {
            Ok(())
        };
        self.frame_depth = 0;
        let flushed = self.flush_frame();
        painted?;
        flushed
    }

    fn flush_frame(&mut self) -> io::Result<()> {
        let mut frame = std::mem::take(&mut *self.frame.borrow_mut());
        let result = if frame.len() > SYNC_BEGIN.len() {
            frame.extend_from_slice(SYNC_END.as_bytes());
            self.write_frame(&frame)
        } else {
            Ok(())
        };
        frame.clear();
        *self.frame.borrow_mut() = frame;
        if result.is_err() {
            // Recorded snapshots describe a frame the terminal never got.
            self.chat_dirty = true;
            self.bottom_dirty = true;
            self.title_activity = None;
        }
        result
    }

    /// Bytes painted into the open frame so far (zero outside a frame).
    fn frame_len(&self) -> usize {
        self.frame.borrow().len()
    }

    #[cfg(test)]
    pub(crate) fn fail_next_write_for_test(&self) {
        self.test_fail_next_write.set(true);
    }

    #[cfg(test)]
    fn writer(&self) -> TestOutput {
        let sink = if self.test_fail_next_write.replace(false) {
            TestSink::Fail
        } else {
            match &self.test_output {
                Some(bytes) => TestSink::Capture(Arc::clone(bytes)),
                None => TestSink::Stdout(io::stdout()),
            }
        };
        TestOutput {
            sink,
            counters: Arc::clone(&self.test_counters),
        }
    }

    /// Output and paint counters accumulated by this renderer.
    #[cfg(test)]
    pub(crate) fn frame_stats(&self) -> FrameStats {
        use std::sync::atomic::Ordering::Relaxed;
        let c = &self.test_counters;
        FrameStats {
            writes: c.writes.load(Relaxed),
            bytes: c.bytes.load(Relaxed),
            flushes: c.flushes.load(Relaxed),
            viewport_paints: c.viewport_paints.load(Relaxed),
            viewport_rows: c.viewport_rows.load(Relaxed),
            bottom_full_paints: c.bottom_full_paints.load(Relaxed),
            bottom_statusline_paints: c.bottom_statusline_paints.load(Relaxed),
            statusline_fits: c.statusline_fits.load(Relaxed),
        }
    }

    /// Set the number of statusline rows (1-3). Call once at startup.
    pub fn set_statusline_height(&mut self, h: usize) {
        self.statusline_height = h.clamp(1, 3);
    }

    pub(crate) fn cached_statusline(
        &mut self,
        key: u64,
        build: impl FnOnce() -> Vec<Vec<StatusSpan>>,
    ) -> Arc<Vec<Vec<StatusSpan>>> {
        if let Some(cache) = &self.statusline_cache
            && cache.key == key
        {
            return Arc::clone(&cache.lines);
        }
        let lines = Arc::new(build());
        self.statusline_cache = Some(StatuslineCache {
            key,
            lines: Arc::clone(&lines),
        });
        #[cfg(test)]
        {
            self.statusline_builds += 1;
        }
        lines
    }

    /// The statusline most recently built for a frame, with its cache key,
    /// or an empty statusline before the first frame. Modal prompts that
    /// repaint the bottom region outside the main frame use it so the
    /// statusline stays visible while they wait.
    pub(crate) fn last_statusline(&self) -> (u64, Arc<Vec<Vec<StatusSpan>>>) {
        match &self.statusline_cache {
            Some(cache) => (cache.key, Arc::clone(&cache.lines)),
            None => (0, Arc::new(Vec::new())),
        }
    }

    #[cfg(test)]
    pub(crate) fn statusline_builds(&self) -> usize {
        self.statusline_builds
    }

    /// Enable activity reporting through the terminal title.
    pub fn set_title_status(&mut self, enabled: bool) {
        self.title_status = enabled;
    }

    /// Enable the opt-in notification and prompt-mark sequences.
    pub(crate) fn set_activity_signals(&mut self, signals: crate::ui::terminal::ActivitySignals) {
        self.activity_signals = signals;
    }

    /// Announce `activity` in the terminal title when it changed, and emit
    /// the enabled notification and prompt-mark sequences for the change.
    /// A no-op unless one of them is enabled.
    pub(crate) fn set_activity(
        &mut self,
        activity: crate::ui::terminal::AgentActivity,
    ) -> io::Result<()> {
        let mut out = if self.activity_signals.any() {
            crate::ui::terminal::activity_sequences(
                self.last_activity,
                activity,
                self.activity_signals,
            )
        } else {
            String::new()
        };
        self.last_activity = Some(activity);
        let title_changed = self.title_status && self.title_activity != Some(activity);
        if title_changed {
            out.push_str(&crate::ui::terminal::title_sequence(activity));
        }
        if out.is_empty() {
            return Ok(());
        }
        self.frame(|this| frame_out!(this).write_all(out.as_bytes()))?;
        if title_changed {
            self.title_activity = Some(activity);
        }
        Ok(())
    }

    /// Show `text` on the status line for [`NOTICE_DURATION`] instead of
    /// adding it to the transcript.
    pub(crate) fn show_notice(&mut self, text: &str, color: Color) {
        self.notice = Some(Notice {
            text: CompactString::from(text),
            color,
            until: std::time::Instant::now() + NOTICE_DURATION,
        });
    }

    /// When the current notice expires, so the UI loop can repaint then.
    pub(crate) fn notice_deadline(&self) -> Option<std::time::Instant> {
        self.notice.as_ref().map(|notice| notice.until)
    }

    /// Text of the notice currently shown, if it has not expired.
    #[cfg(test)]
    pub(crate) fn notice_text(&self) -> Option<&str> {
        self.active_notice().map(|notice| notice.text.as_str())
    }

    fn active_notice(&self) -> Option<&Notice> {
        self.notice
            .as_ref()
            .filter(|notice| notice.until > std::time::Instant::now())
    }

    /// Rows reserved at the bottom: statusline lines + separator + input baseline.
    fn statusline_reserve(&self) -> u16 {
        self.statusline_height as u16 + 2
    }

    pub fn set_monochrome(&mut self, monochrome: bool) {
        self.feed.set_monochrome(monochrome);
        self.monochrome = monochrome;
        self.statusline_rows.borrow_mut().clear();
    }

    /// Set the chat buffer's left padding in columns. Clamped so content keeps
    /// at least a few usable columns.
    pub fn set_chat_margin(&mut self, margin: u16) {
        let (cols, _) = self.terminal_size();
        self.chat_margin = margin.min(cols.saturating_sub(8));
    }

    /// Emit the chat left-margin gutter (spaces in the chat background) at the
    /// current cursor position. Caller has already positioned to column 0 and
    /// set the background.
    fn write_chat_margin(&self, stdout: &mut impl Write) -> io::Result<()> {
        if self.chat_margin > 0 {
            write!(stdout, "{}", " ".repeat(self.chat_margin as usize))?;
        }
        Ok(())
    }

    pub fn set_background_colors(
        &mut self,
        chat_bg: Option<Color>,
        input_bg: Option<Color>,
        status_bg: Option<Color>,
    ) {
        self.chat_bg = chat_bg;
        self.input_bg = input_bg;
        self.status_bg = status_bg;
        self.statusline_rows.borrow_mut().clear();
    }

    pub(crate) fn set_foreground_palette(&mut self, palette: ForegroundPalette) {
        if self.palette == palette {
            return;
        }
        self.palette = palette;
        self.feed.set_palette(palette);
        self.statusline_rows.borrow_mut().clear();
        self.last_chat_snapshot = None;
        self.last_bottom_snapshot = None;
    }

    pub(crate) fn set_terminal_background(&mut self, color: Option<Color>) {
        self.terminal_background = color;
    }

    pub(crate) fn terminal_background(&self) -> Option<Color> {
        self.terminal_background
    }

    #[cfg(test)]
    pub(crate) fn foreground_palette_for_test(&self) -> ForegroundPalette {
        self.palette
    }

    #[cfg(test)]
    pub(crate) fn background_colors_for_test(
        &self,
    ) -> (Option<Color>, Option<Color>, Option<Color>) {
        (self.chat_bg, self.input_bg, self.status_bg)
    }

    fn color(&self, color: Color) -> Color {
        resolve_color(color, self.monochrome)
    }

    /// Geometry owned by a captured App fixture, which never queries the
    /// process's real terminal.
    #[cfg(test)]
    fn fixture_size(&self) -> Option<(u16, u16)> {
        (self.test_output.is_some() && !self.test_native_geometry).then_some((180, 50))
    }

    /// One native terminal-size query (an ioctl on Unix).
    fn query_native_size(&self) -> io::Result<(u16, u16)> {
        #[cfg(test)]
        {
            self.native_size_queries
                .set(self.native_size_queries.get() + 1);
            if let Some(query) = self.test_native_size_query {
                return query();
            }
        }
        crossterm::terminal::size()
    }

    /// The terminal geometry layout works against: the size cached by the
    /// last bottom draw or resize, queried natively only when nothing is
    /// cached yet. Falls back to 80x24 when the query fails.
    fn terminal_size(&self) -> (u16, u16) {
        #[cfg(test)]
        if let Some(size) = self.fixture_size() {
            return size;
        }
        if let Some(size) = self.cached_size.get() {
            return size;
        }
        match self.query_native_size() {
            Ok(size) => {
                self.cached_size.set(Some(size));
                size
            }
            Err(_) => (80, 24),
        }
    }

    /// Bottom drawing queries the terminal once per frame and preserves
    /// native query errors; the result refreshes the cached geometry. A size
    /// that changed without a resize event marks the chat dirty so the next
    /// frame lays it out again.
    fn checked_draw_size(&mut self) -> io::Result<(u16, u16)> {
        #[cfg(test)]
        if let Some(size) = self.fixture_size() {
            return Ok(size);
        }
        let size = self.query_native_size()?;
        if self
            .cached_size
            .replace(Some(size))
            .is_some_and(|old| old != size)
        {
            self.chat_dirty = true;
        }
        Ok(size)
    }

    #[cfg(test)]
    pub(crate) fn draw_captured_picker(
        &mut self,
        paint: impl FnOnce(u16) -> io::Result<()>,
    ) -> Option<io::Result<()>> {
        let bytes = self.test_output.as_ref()?.clone();
        if super::pickers::test_surface::native_only() {
            return None;
        }
        let size = match self.checked_draw_size() {
            Ok(size) => size,
            Err(error) => return Some(Err(error)),
        };
        let floor = self.picker_floor_row();
        Some(super::pickers::test_surface::capture(bytes, size, || {
            paint(floor)
        }))
    }

    pub(crate) fn draw_picker_with(
        &mut self,
        paint: impl FnOnce(&mut dyn std::io::Write, u16, u16, Option<Color>) -> io::Result<()>,
    ) -> io::Result<()> {
        let (cols, _) = self.checked_draw_size()?;
        let floor = self.picker_floor_row();
        let bg = self.chat_bg;
        self.frame(|this| paint(&mut frame_out!(this), cols, floor, bg))
    }

    fn max_line_width(&self) -> usize {
        let (cols, _) = self.terminal_size();
        cols.saturating_sub(1 + self.chat_margin) as usize
    }

    #[cfg(test)]
    pub fn line_width(&self) -> usize {
        self.max_line_width()
    }

    /// The laid-out chat rows at `width` (shared with the feed's layout
    /// cache, so this is a cheap `Arc` clone when nothing changed).
    fn chat_lines(&self, width: usize) -> Arc<FeedLines> {
        self.feed.lines(width)
    }

    pub fn buffer_len(&self) -> usize {
        self.feed.lines(self.max_line_width()).len()
    }

    /// Access the underlying feed for callers that want to push semantic blocks
    /// directly (e.g., session rendering or streaming agent responses).
    pub fn feed(&self) -> &Feed {
        &self.feed
    }

    pub fn feed_mut(&mut self) -> &mut Feed {
        &mut self.feed
    }

    /// Snapshot of everything the chat viewport currently paints.
    fn chat_snapshot(&self) -> ChatSnapshot {
        ChatSnapshot {
            feed_generation: self.feed.generation(),
            width: self.max_line_width(),
            visible_rows: self.visible_lines(),
            scroll_offset: self.scroll_offset,
            selection_active: self.selection_active,
            selection_start: self.selection_start,
            selection_end: self.selection_end,
            chat_bg: self.chat_bg,
            monochrome: self.monochrome,
        }
    }

    /// Whether the chat viewport needs repainting: either a renderer-internal
    /// mutation marked it dirty, or the tracked state changed since the last
    /// recorded draw. The state comparison also catches feed mutations made
    /// through `feed_mut()` and direct writes to the public selection fields.
    pub fn chat_needs_redraw(&self) -> bool {
        if self.chat_dirty {
            return true;
        }
        match &self.last_chat_snapshot {
            Some(prev) => *prev != self.chat_snapshot(),
            None => true,
        }
    }

    /// Record the current chat state as freshly drawn.
    fn record_chat_drawn(&mut self) {
        self.last_chat_snapshot = Some(self.chat_snapshot());
        self.chat_dirty = false;
    }

    /// Test helper: mark the chat viewport clean without drawing, as if a
    /// `render_viewport` had just completed.
    #[cfg(test)]
    pub fn mark_chat_clean(&mut self) {
        self.record_chat_drawn();
    }

    /// Mark both regions dirty, forcing a full repaint on the next frame.
    /// Used when something painted over the screen outside the tracked paths
    /// (e.g. an active picker overlay).
    pub fn invalidate(&mut self) {
        self.chat_dirty = true;
        self.bottom_dirty = true;
    }

    /// Forget the announced title activity: a suspended and resumed terminal
    /// restored the pre-attach title, so the next frame announces again.
    /// The cached geometry is dropped too: resize events are not read while
    /// the terminal is handed to another program.
    pub(crate) fn forget_title(&mut self) {
        self.title_activity = None;
        self.cached_size.set(None);
    }

    /// The separator row above the input: the first row, counting down, that a
    /// picker overlay must leave alone. Tracks the statusline height and the
    /// current input height, so a taller bottom region lifts the overlay.
    pub(crate) fn picker_floor_row(&self) -> u16 {
        let (_, rows) = self.terminal_size();
        input_top_row(
            rows,
            self.statusline_reserve(),
            self.prev_input_height.max(1),
        )
        .saturating_sub(1)
    }

    pub fn visible_lines(&self) -> usize {
        let (_, rows) = self.terminal_size();
        let input_height = self.prev_input_height.max(1);
        rows.saturating_sub(input_height as u16 + self.statusline_reserve()) as usize
    }

    /// Number of rows the input area will occupy for the given content. Kept in
    /// sync with the height logic used while drawing the input in `draw_bottom`.
    fn input_visible_height(&self, input_line: &str, rows: u16) -> usize {
        if let Some(prompt_rows) = self.overlay_prompt_rows(self.terminal_size().0, rows) {
            return prompt_rows.len();
        }
        let available_rows = rows.saturating_sub(self.statusline_reserve()) as usize;
        let max_input_rows = available_rows.min((available_rows * 3 / 10).max(5));
        let (cols, _) = self.terminal_size();
        crate::ui::input::wrap::wrap_rows(input_line, input_text_width(cols))
            .len()
            .min(max_input_rows)
            .max(1)
    }

    /// Display width the input text soft-wraps at (the terminal width less
    /// the prompt), for the editor's Up/Down row movement.
    pub fn input_wrap_width(&self) -> usize {
        input_text_width(self.terminal_size().0)
    }

    /// Options row of the chain prompt for its current mode.
    fn chain_options(&self) -> &'static str {
        if self.chain_but_mode {
            "[Enter] send  [Esc] cancel"
        } else {
            "[Y] Yes  [N] No  [B] yes, But (add instruction)"
        }
    }

    /// Rows of the active permission or chain prompt laid out for a
    /// `cols` x `rows` terminal, or `None` when the input editor is shown.
    fn overlay_prompt_rows(&self, cols: u16, rows: u16) -> Option<Vec<String>> {
        let available = rows.saturating_sub(self.statusline_reserve()) as usize;
        // One column short of the edge: a full-width row leaves the terminal
        // in its pending-wrap state, where clearing to end of line misbehaves.
        let width = (cols as usize).saturating_sub(1).max(1);
        if let Some(pp) = &self.permission_prompt {
            Some(permission_prompt_rows(
                &pp.tool,
                &pp.options,
                width,
                available,
                pp.expanded,
            ))
        } else {
            self.chain_prompt.as_ref().map(|cp| {
                prompt_block_rows(
                    &cp.question,
                    self.chain_options(),
                    width,
                    prompt_max_rows(available),
                )
            })
        }
    }

    /// Recompute the input height and reconcile `prev_input_height` before the
    /// chat viewport is drawn, so the chat is sized against the height the input
    /// is about to use. Without this, a height change (e.g. clearing or pasting
    /// text) leaves the viewport drawn for the old size until the next redraw.
    pub fn sync_input_height(&mut self, input_line: &str) -> io::Result<()> {
        let (_, rows) = self.terminal_size();
        let new_height = self.input_visible_height(input_line, rows);
        let old_height = self.prev_input_height;
        self.prev_input_height = new_height;
        if new_height < old_height {
            self.frame(|this| this.clear_shrunk_rows(old_height, new_height))?;
        }
        Ok(())
    }

    pub fn buffer_line_at_row(&self, row: u16) -> Option<usize> {
        let width = self.max_line_width();
        let total = self.chat_lines(width).len();
        if total == 0 {
            return None;
        }

        let visible = self.visible_lines();
        let auto_scroll = self.scroll_offset == 0;
        let pad = if auto_scroll && total < visible {
            visible - total
        } else {
            0
        };
        if (row as usize) < pad {
            return None;
        }

        let start = if auto_scroll {
            total.saturating_sub(visible)
        } else {
            total.saturating_sub((self.scroll_offset + visible).min(total))
        };
        let start = start.min(total.saturating_sub(visible));

        Some(start + (row as usize) - pad)
    }

    pub fn clear_selection(&mut self) {
        self.chat_dirty = true;
        self.selection_active = false;
        self.selection_start = None;
        self.selection_end = None;
        self.selection_dragged = false;
        self.selection_word = false;
    }

    pub(crate) fn toggle_reasoning_at_line(&mut self, line: usize) -> bool {
        let width = self.max_line_width();
        if self.feed.toggle_reasoning_at_row(width, line) {
            self.clear_selection();
            self.chat_dirty = true;
            true
        } else {
            false
        }
    }

    /// Record a transcript press at `(row, col)` and report whether it
    /// completes a double-click. A double-click is consumed, so a third
    /// press starts over.
    pub(crate) fn register_click(&mut self, row: u16, col: u16) -> bool {
        let now = std::time::Instant::now();
        if is_double_click(self.last_click, now, row, col) {
            self.last_click = None;
            return true;
        }
        self.last_click = Some(ClickAt { at: now, row, col });
        false
    }

    /// Select the word under display column `col` of laid-out chat line
    /// `line`. Returns `false`, selecting nothing, when there is no word
    /// there.
    pub(crate) fn select_word_at(&mut self, line: usize, col: usize) -> bool {
        let lines = self.chat_lines(self.max_line_width());
        let Some((start, end)) = lines
            .get(line)
            .and_then(|entry| word_cols_at(&entry.text, col))
        else {
            return false;
        };
        self.selection_active = true;
        self.selection_start = Some(SelectionPoint::new(line, start));
        self.selection_end = Some(SelectionPoint::new(line, end));
        self.selection_dragged = false;
        self.selection_word = true;
        self.chat_dirty = true;
        true
    }

    pub fn link_url_at(&self, buf_idx: usize, col: u16) -> Option<String> {
        let lines = self.chat_lines(self.max_line_width());
        let entry = lines.get(buf_idx)?;
        let text: &str = &entry.text;
        let click_col = col.saturating_sub(self.chat_margin) as usize;
        for m in URL_RE.find_iter(text) {
            let prefix = &text[..m.start()];
            let url_start = display_width(prefix);
            let url_end = url_start + display_width(m.as_str());
            if click_col >= url_start && click_col < url_end {
                return Some(m.as_str().to_string());
            }
        }
        None
    }

    /// Map a mouse column to a display column within a chat line's text.
    pub fn chat_text_col(&self, col: u16) -> usize {
        col.saturating_sub(self.chat_margin) as usize
    }

    /// The selected text, sliced at the selection's columns: partial first
    /// and last lines, whole lines between them.
    pub fn selected_text(&self) -> Option<String> {
        let (Some(a), Some(b)) = (self.selection_start, self.selection_end) else {
            return None;
        };
        let lines = self.chat_lines(self.max_line_width());
        let (lo, hi) = (a.min(b).line, a.max(b).line);
        let pieces: Vec<&str> = (lo..=hi)
            .filter_map(|line| {
                let text: &str = &lines.get(line)?.text;
                let (start, end) = selection_byte_range(text, line, a, b)?;
                Some(&text[start..end])
            })
            .collect();
        let result = pieces.join("\n");
        (!result.is_empty()).then_some(result)
    }

    pub fn is_scrolling(&self) -> bool {
        self.scroll_offset > 0
    }

    /// Map a mouse click at `(row, col)` to a cursor byte offset inside the
    /// input buffer, or `None` if the click falls outside the input area.
    pub fn input_cursor_for_click(&self, row: u16, col: u16, input_line: &str) -> Option<usize> {
        let vlc = self.input_visible_line_count;
        if vlc == 0 {
            return None;
        }
        if row < self.input_base_row || row >= self.input_base_row + vlc as u16 {
            return None;
        }
        let visible_idx = (row - self.input_base_row) as usize;
        let row_idx = self.input_first_visible + visible_idx;
        let (cols, _) = self.terminal_size();
        let layout = crate::ui::input::wrap::wrap_rows(input_line, input_text_width(cols));
        layout.get(row_idx)?;
        // Display column the click lands on, within the row's text. Clicks on
        // the prompt (or to its left) snap to the start of the row.
        let target_display = (col as usize).saturating_sub(self.input_prompt_width);
        Some(crate::ui::input::wrap::offset_in_row(
            input_line,
            &layout,
            row_idx,
            target_display,
        ))
    }

    /// Scroll the multi-line input viewport up one line (toward earlier lines).
    /// Returns false when the input is already showing its top line, so the
    /// caller can fall through to scrolling the chat history instead.
    pub fn input_scroll_up(&mut self) -> bool {
        if self.input_vscroll_offset > 0 {
            self.input_vscroll_offset -= 1;
            true
        } else {
            false
        }
    }

    /// Scroll the multi-line input viewport down one line (toward the end).
    /// Returns false when the input is already at the bottom.
    pub fn input_scroll_down(&mut self) -> bool {
        if self.input_vscroll_offset < self.input_max_vscroll {
            self.input_vscroll_offset += 1;
            true
        } else {
            false
        }
    }

    pub fn scroll_line_up(&mut self) {
        let visible = self.visible_lines();
        let max_offset = self.buffer_len().saturating_sub(visible);
        if self.scroll_offset < max_offset {
            self.scroll_offset += 1;
            self.chat_dirty = true;
        }
    }

    pub fn scroll_line_down(&mut self) {
        if self.scroll_offset > 0 {
            self.scroll_offset -= 1;
            self.chat_dirty = true;
        }
    }

    pub fn scroll_page_up(&mut self) {
        let visible = self.visible_lines();
        let page = visible.saturating_sub(2).max(1);
        let max_offset = self.buffer_len().saturating_sub(visible);
        let new_offset = (self.scroll_offset + page).min(max_offset);
        if new_offset != self.scroll_offset {
            self.scroll_offset = new_offset;
            self.chat_dirty = true;
        }
    }

    pub fn scroll_page_down(&mut self) {
        let visible = self.visible_lines();
        let page = visible.saturating_sub(2).max(1);
        let new_offset = if self.scroll_offset <= page {
            0
        } else {
            self.scroll_offset.saturating_sub(page)
        };
        if new_offset != self.scroll_offset {
            self.scroll_offset = new_offset;
            self.chat_dirty = true;
        }
    }

    pub fn scroll_to_top(&mut self) {
        let visible = self.visible_lines();
        let new_offset = self.buffer_len().saturating_sub(visible);
        if new_offset != self.scroll_offset {
            self.scroll_offset = new_offset;
            self.chat_dirty = true;
        }
    }

    pub fn scroll_to_bottom(&mut self) -> io::Result<()> {
        if self.scroll_offset != 0 {
            self.scroll_offset = 0;
            self.chat_dirty = true;
        }
        self.sync_to_buffer()
    }

    fn sync_to_buffer(&mut self) -> io::Result<()> {
        self.render_viewport()
    }

    /// Repaint the chat viewport when it changed since the last draw.
    pub fn render_viewport(&mut self) -> io::Result<()> {
        self.frame(Self::render_viewport_in_frame)
    }

    fn render_viewport_in_frame(&mut self) -> io::Result<()> {
        self.reconcile_feed_retention();
        if !self.chat_needs_redraw() {
            return Ok(());
        }
        let trace = FrameTrace::start("viewport");
        let frame_start = self.frame_len();
        let (cols, _rows) = self.terminal_size();
        let max_width = cols.saturating_sub(1 + self.chat_margin) as usize;
        let visible = self.visible_lines();
        let buffer = self.chat_lines(max_width);
        let total = buffer.len();
        // The viewport may have grown since the offset was set (the input box
        // shrank, the terminal was resized); keep the offset inside the
        // scrollable range so the arithmetic below never underflows.
        self.scroll_offset = clamp_scroll_offset(self.scroll_offset, total, visible);
        let mut stdout = frame_out!(self);
        write!(stdout, "{}", Hide)?;

        let auto_scroll = self.scroll_offset == 0;
        let start = if auto_scroll {
            total.saturating_sub(visible)
        } else {
            total.saturating_sub(self.scroll_offset + visible)
        };
        let start = start.min(total.saturating_sub(visible));

        let mut visual_row: u16 = 0;
        let mut buf_idx = start;

        // Bottom-align: when auto-scrolling and content is shorter than viewport,
        // render empty rows first so content hugs the input area.
        if auto_scroll && total < visible {
            let pad = visible - total;
            for _ in 0..pad {
                stdout.queue(MoveTo(0, visual_row))?;
                if let Some(bg) = self.chat_bg {
                    write!(stdout, "{}", SetBackgroundColor(self.color(bg)))?;
                }
                write!(stdout, "{}", Clear(ClearType::UntilNewLine))?;
                write!(stdout, "{}", ResetColor)?;
                visual_row += 1;
            }
        }

        while (visual_row as usize) < visible && buf_idx < total {
            let entry = &buffer[buf_idx];
            let chunk = &entry.text;

            if (visual_row as usize) >= visible {
                break;
            }

            stdout.queue(MoveTo(0, visual_row))?;

            let selected = match (
                self.selection_active,
                self.selection_start,
                self.selection_end,
            ) {
                (true, Some(a), Some(b)) => selection_byte_range(chunk, buf_idx, a, b),
                _ => None,
            };

            if let Some(bg) = self.chat_bg {
                write!(stdout, "{}", SetBackgroundColor(self.color(bg)))?;
            }
            self.write_chat_margin(&mut stdout)?;
            write!(stdout, "{}", SetForegroundColor(self.color(entry.color)))?;
            paint_styled_row(&mut stdout, entry, selected, self.monochrome)?;
            write!(stdout, "{}", Clear(ClearType::UntilNewLine))?;
            write!(stdout, "{}", ResetColor)?;

            visual_row += 1;
            buf_idx += 1;
        }

        while (visual_row as usize) < visible {
            stdout.queue(MoveTo(0, visual_row))?;
            if let Some(bg) = self.chat_bg {
                write!(stdout, "{}", SetBackgroundColor(self.color(bg)))?;
            }
            write!(stdout, "{}", Clear(ClearType::UntilNewLine))?;
            write!(stdout, "{}", ResetColor)?;
            visual_row += 1;
        }

        if let Some(indicator) = history_indicator(self.scroll_offset, total, visible) {
            let (text, color) = match &indicator {
                HistoryIndicator::Scrolled(text) => (text, Color::DarkYellow),
                HistoryIndicator::Above(text) => (text, Color::DarkGrey),
            };
            let x = cols.saturating_sub(display_width(text) as u16);
            stdout.queue(MoveTo(x, 0))?;
            if let Some(bg) = self.chat_bg {
                write!(stdout, "{}", SetBackgroundColor(self.color(bg)))?;
            }
            write!(stdout, "{}", SetForegroundColor(self.color(color)))?;
            write!(stdout, "{}", text)?;
            write!(stdout, "{}", ResetColor)?;
        }

        #[cfg(test)]
        {
            OutputCounters::bump(&self.test_counters.viewport_paints, 1);
            OutputCounters::bump(&self.test_counters.viewport_rows, visible);
        }
        trace.finish(self.frame_len() - frame_start);
        self.record_chat_drawn();
        Ok(())
    }

    pub(crate) fn reconcile_feed_retention(&mut self) {
        let retention_generation = self.feed.retention_generation();
        if retention_generation != self.seen_feed_retention_generation {
            self.seen_feed_retention_generation = retention_generation;
            self.scroll_offset = 0;
            self.selection_active = false;
            self.selection_start = None;
            self.selection_end = None;
            self.selection_dragged = false;
            self.selection_word = false;
            self.chat_dirty = true;
        }
    }

    /// Show a line just written: paint the chat now when following the
    /// tail, or once when the enclosing frame closes. Painting immediately
    /// outside a frame keeps a line visible across an await that follows it
    /// (a prompt waiting for the user, a slow network call).
    fn paint_written_line(&mut self) -> io::Result<()> {
        if self.scroll_offset != 0 {
            return Ok(());
        }
        if self.frame_depth > 0 {
            self.deferred_line_paint = true;
            return Ok(());
        }
        self.render_viewport()
    }

    pub fn write_line(&mut self, text: &str, color: Color) -> io::Result<()> {
        let style = style_from_color(color);
        self.feed.push_block(style, text);
        self.chat_dirty = true;
        self.paint_written_line()
    }

    /// Like [`Renderer::write_line`], and later lines can be placed under
    /// this one with [`Renderer::write_line_after`] using the same `anchor`.
    pub fn write_line_anchored(
        &mut self,
        anchor: &str,
        text: &str,
        color: Color,
    ) -> io::Result<()> {
        self.feed
            .push_anchored_block(anchor, style_from_color(color), text);
        self.chat_dirty = true;
        self.paint_written_line()
    }

    /// Place a line directly under the line written with `anchor` (a tool
    /// result under its own call), or at the end when that line is gone.
    pub fn write_line_after(&mut self, anchor: &str, text: &str, color: Color) -> io::Result<()> {
        let style = style_from_color(color);
        self.feed.place_after_anchor(anchor, style, text);
        self.chat_dirty = true;
        self.paint_written_line()
    }

    pub fn clear_content(&mut self) -> io::Result<()> {
        self.frame(Self::clear_content_in_frame)
    }

    fn clear_content_in_frame(&mut self) -> io::Result<()> {
        self.chat_dirty = true;
        self.feed.clear();
        self.scroll_offset = 0;
        self.clear_selection();
        let mut stdout = frame_out!(self);
        if let Some(bg) = self.chat_bg {
            write!(stdout, "{}", SetBackgroundColor(self.color(bg)))?;
        }
        stdout.queue(Clear(ClearType::All))?;
        write!(stdout, "{}", ResetColor)?;
        stdout.queue(MoveTo(0, 0))?;
        Ok(())
    }

    pub fn resize(&mut self) {
        self.chat_dirty = true;
        // The terminal changed size: forget the cached geometry so this
        // frame's layout queries it once.
        self.cached_size.set(None);
        let visible = self.visible_lines();
        let max_offset = self.buffer_len().saturating_sub(visible);
        if self.scroll_offset > max_offset {
            self.scroll_offset = max_offset;
        }
    }

    fn clear_shrunk_rows(&self, old_height: usize, new_height: usize) -> io::Result<()> {
        if new_height >= old_height {
            return Ok(());
        }
        let (_, rows) = self.terminal_size();
        let reserve = self.statusline_reserve();
        let avail = rows.saturating_sub(reserve);
        let old_start = avail.saturating_sub(old_height as u16).saturating_add(1);
        let new_start = avail.saturating_sub(new_height as u16).saturating_add(1);
        let mut stdout = frame_out!(self);
        for row in old_start..new_start {
            stdout.queue(MoveTo(0, row))?;
            if let Some(bg) = self.input_bg {
                write!(stdout, "{}", SetBackgroundColor(self.color(bg)))?;
            }
            write!(stdout, "{}", Clear(ClearType::UntilNewLine))?;
            write!(stdout, "{}", ResetColor)?;
        }
        Ok(())
    }

    fn draw_separator(&self, row: u16, cols: u16) -> io::Result<()> {
        let mut stdout = frame_out!(self);
        stdout.queue(MoveTo(0, row))?;
        if let Some(bg) = self.input_bg {
            write!(stdout, "{}", SetBackgroundColor(self.color(bg)))?;
        }
        write!(
            stdout,
            "{}",
            SetForegroundColor(self.color(self.palette.system))
        )?;
        let sep: String = "─".repeat(cols as usize);
        write!(stdout, "{}", sep)?;
        write!(stdout, "{}", ResetColor)?;
        Ok(())
    }

    /// Draw the statusline (1-3 lines) at the bottom rows. Each line's `Flex` spans
    /// expand to fill remaining width. Fewer lines than `statusline_height` leaves
    /// the upper statusline rows blank.
    fn draw_statusline(
        &self,
        statusline: &[Vec<StatusSpan>],
        statusline_key: u64,
        cols: u16,
        is_scrolling: bool,
    ) -> io::Result<()> {
        let (_, rows) = self.terminal_size();
        let h = self.statusline_height as u16;
        let mut cache = self.statusline_rows.borrow_mut();
        cache.resize_with(h as usize, || None);
        let mut out = frame_out!(self);
        for row_idx in 0..h {
            let screen_row = rows.saturating_sub(h - row_idx);
            let empty: Vec<StatusSpan> = Vec::new();
            let spans = statusline.get(row_idx as usize).unwrap_or(&empty);
            // A transient notice, else the scroll indicator, on the top
            // statusline row only.
            let notice = self
                .active_notice()
                .map(|notice| (format!("{}  ", notice.text), notice.color));
            let prefix = match (row_idx, notice) {
                (0, Some((text, color))) => Some((text, color)),
                (0, None) if is_scrolling => Some(("-- SCROLL -- ".to_string(), Color::DarkYellow)),
                _ => None,
            };
            let prefix = prefix.as_ref().map(|(text, color)| (text.as_str(), *color));
            // The painted row depends only on the statusline content (its
            // cache key), the geometry and the prefix; theme setters clear
            // the cache. An unchanged row is one copy into the frame.
            let row_key = hash_value((statusline_key, cols, screen_row, prefix));
            let slot = &mut cache[row_idx as usize];
            if slot.as_ref().is_none_or(|row| row.key != row_key) {
                let mut bytes = Vec::with_capacity(256);
                self.draw_statusline_row(&mut bytes, screen_row, spans, prefix, cols)?;
                *slot = Some(FittedStatusRow {
                    key: row_key,
                    bytes,
                });
            }
            if let Some(row) = slot {
                out.write_all(&row.bytes)?;
            }
        }
        Ok(())
    }

    /// Fit and paint one statusline row into `stdout`.
    fn draw_statusline_row(
        &self,
        stdout: &mut impl Write,
        screen_row: u16,
        spans: &[StatusSpan],
        prefix: Option<(&str, Color)>,
        cols: u16,
    ) -> io::Result<()> {
        #[cfg(test)]
        OutputCounters::bump(&self.test_counters.statusline_fits, 1);
        stdout.queue(MoveTo(0, screen_row))?;
        if let Some(bg) = self.status_bg {
            write!(stdout, "{}", SetBackgroundColor(self.color(bg)))?;
        }
        write!(stdout, "{}", Clear(ClearType::CurrentLine))?;
        stdout.queue(MoveTo(0, screen_row))?;
        if let Some(bg) = self.status_bg {
            write!(stdout, "{}", SetBackgroundColor(self.color(bg)))?;
        }

        // Reserve the last cell even on legacy consoles with eager autowrap.
        let total = cols.saturating_sub(1) as usize;
        let mut budget = total;

        if let Some((prefix, color)) = prefix {
            write!(stdout, "{}", SetForegroundColor(self.color(color)))?;
            let take = super::statusline::ellipsize(prefix, budget);
            budget -= display_width(&take);
            write!(stdout, "{}", take)?;
        }

        let fitted = super::statusline::fit_line(spans, budget);
        let spans: &[StatusSpan] = &fitted;

        // Fixed width of all text spans; flex shares what is left.
        let fixed: usize = spans
            .iter()
            .map(|s| match s {
                StatusSpan::Text { text, .. } => display_width(text),
                StatusSpan::Flex => 0,
            })
            .sum();
        let flex_count = spans
            .iter()
            .filter(|s| matches!(s, StatusSpan::Flex))
            .count();
        let flex_left = budget.saturating_sub(fixed);
        let mut flex_seen = 0usize;

        for span in spans {
            if budget == 0 {
                break;
            }
            match span {
                StatusSpan::Text {
                    text,
                    fg,
                    bg,
                    layout,
                } => {
                    let bgc = bg.or(self.status_bg);
                    if let Some(c) = bgc {
                        write!(stdout, "{}", SetBackgroundColor(self.color(c)))?;
                    }
                    let fgc = self.status_foreground(layout, *fg);
                    write!(stdout, "{}", SetForegroundColor(self.color(fgc)))?;
                    debug_assert!(display_width(text) <= budget);
                    budget = budget.saturating_sub(display_width(text));
                    write!(stdout, "{}", text)?;
                    write!(stdout, "{}", ResetColor)?;
                    if let Some(bg) = self.status_bg {
                        write!(stdout, "{}", SetBackgroundColor(self.color(bg)))?;
                    }
                }
                StatusSpan::Flex => {
                    flex_seen += 1;
                    if flex_count == 0 {
                        continue;
                    }
                    // Distribute leftover evenly; earliest flex absorbs the remainder.
                    let base = flex_left / flex_count;
                    let extra = if flex_seen <= flex_left % flex_count {
                        1
                    } else {
                        0
                    };
                    let width = (base + extra).min(budget);
                    budget = budget.saturating_sub(width);
                    write!(stdout, "{}", " ".repeat(width))?;
                }
            }
        }

        write!(stdout, "{}", Clear(ClearType::UntilNewLine))?;
        write!(stdout, "{}", ResetColor)?;
        Ok(())
    }

    /// Paint one statusline row as its own frame.
    #[cfg(test)]
    fn statusline_row_frame_for_test(
        &mut self,
        screen_row: u16,
        spans: &[StatusSpan],
        prefix: Option<(&str, Color)>,
        cols: u16,
    ) -> io::Result<()> {
        self.frame(|this| {
            this.draw_statusline_row(&mut frame_out!(this), screen_row, spans, prefix, cols)
        })
    }

    fn status_foreground(
        &self,
        layout: &super::statusline::SpanLayout,
        fg: Option<Color>,
    ) -> Color {
        match (layout, fg) {
            (
                super::statusline::SpanLayout::Item {
                    dynamic_color: true,
                    ..
                },
                Some(Color::Red),
            ) => self.palette.error,
            (
                super::statusline::SpanLayout::Item {
                    dynamic_color: true,
                    ..
                },
                Some(Color::Yellow),
            ) => self.palette.warning,
            _ => fg.unwrap_or(self.palette.status),
        }
    }

    /// Snapshot of everything `draw_bottom` would paint for these arguments.
    fn bottom_snapshot(
        &self,
        input_line: &str,
        cursor_pos: usize,
        statusline_key: u64,
        is_running: bool,
        cols: u16,
        rows: u16,
    ) -> BottomSnapshot {
        let prompt = if let Some(ref pp) = self.permission_prompt {
            PromptSnapshot::Permission {
                tool: pp.tool.clone(),
                options: pp.options.clone(),
                expanded: pp.expanded,
            }
        } else if let Some(ref cp) = self.chain_prompt {
            PromptSnapshot::Chain {
                question: cp.question.clone(),
                but_mode: self.chain_but_mode,
            }
        } else {
            PromptSnapshot::Input
        };
        BottomSnapshot {
            cols,
            rows,
            statusline_height: self.statusline_height,
            input_hash: hash_value(input_line),
            cursor_pos,
            is_running,
            spinner_frame: self.spinner_frame,
            input_vscroll_offset: self.input_vscroll_offset,
            scroll_indicator: matches!(prompt, PromptSnapshot::Input) && self.scroll_offset > 0,
            prompt,
            statusline_key,
            monochrome: self.monochrome,
            input_bg: self.input_bg,
            status_bg: self.status_bg,
            notice: self.active_notice().map(|notice| notice.text.clone()),
        }
    }

    /// Pure redraw decision for the bottom region: compare the state recorded
    /// after the last draw with the state about to be drawn. When only the
    /// statusline content (or the scroll indicator painted inside it) differs,
    /// the input area is untouched and only the statusline rows need a repaint.
    pub(crate) fn bottom_redraw_plan(
        prev: Option<&BottomSnapshot>,
        next: &BottomSnapshot,
        force_full: bool,
    ) -> BottomRedrawPlan {
        if force_full {
            return BottomRedrawPlan::Full;
        }
        let Some(prev) = prev else {
            return BottomRedrawPlan::Full;
        };
        if prev == next {
            return BottomRedrawPlan::Skip;
        }
        if next.cols == prev.cols
            && next.rows == prev.rows
            && next.statusline_height == prev.statusline_height
            && next.input_hash == prev.input_hash
            && next.cursor_pos == prev.cursor_pos
            && next.is_running == prev.is_running
            && next.spinner_frame == prev.spinner_frame
            && next.input_vscroll_offset == prev.input_vscroll_offset
            && next.prompt == prev.prompt
            && next.monochrome == prev.monochrome
            && next.input_bg == prev.input_bg
            && next.status_bg == prev.status_bg
            && (next.statusline_key != prev.statusline_key
                || next.scroll_indicator != prev.scroll_indicator
                || next.notice != prev.notice)
        {
            BottomRedrawPlan::StatuslineOnly
        } else {
            BottomRedrawPlan::Full
        }
    }

    /// Record the bottom region as freshly drawn.
    fn record_bottom_drawn(&mut self, snapshot: BottomSnapshot) {
        self.last_bottom_snapshot = Some(snapshot);
        self.bottom_dirty = false;
    }

    /// Re-place the terminal caret where the last full bottom draw left it.
    /// Needed after a statusline-only redraw or a picker overlay, both of
    /// which move the cursor.
    pub(crate) fn restore_bottom_cursor(&mut self) -> io::Result<()> {
        self.frame(|this| this.write_bottom_cursor())
    }

    fn write_bottom_cursor(&self) -> io::Result<()> {
        let mut stdout = frame_out!(self);
        match self.bottom_cursor {
            Some((x, row)) => {
                stdout.queue(MoveTo(x, row))?;
                write!(stdout, "{}", Show)?;
            }
            None => {
                write!(stdout, "{}", Hide)?;
            }
        }
        Ok(())
    }

    pub fn draw_bottom(
        &mut self,
        input_line: &str,
        cursor_pos: usize,
        statusline: &[Vec<StatusSpan>],
        statusline_key: u64,
        is_running: bool,
    ) -> io::Result<()> {
        self.frame(|this| {
            this.draw_bottom_in_frame(
                input_line,
                cursor_pos,
                statusline,
                statusline_key,
                is_running,
            )
        })
    }

    fn draw_bottom_in_frame(
        &mut self,
        input_line: &str,
        cursor_pos: usize,
        statusline: &[Vec<StatusSpan>],
        statusline_key: u64,
        is_running: bool,
    ) -> io::Result<()> {
        let (cols, rows) = self.checked_draw_size()?;
        if self.notice.is_some() && self.active_notice().is_none() {
            self.notice = None;
        }
        let snapshot = self.bottom_snapshot(
            input_line,
            cursor_pos,
            statusline_key,
            is_running,
            cols,
            rows,
        );
        match Self::bottom_redraw_plan(
            self.last_bottom_snapshot.as_ref(),
            &snapshot,
            self.bottom_dirty,
        ) {
            BottomRedrawPlan::Skip => return Ok(()),
            BottomRedrawPlan::StatuslineOnly => {
                #[cfg(test)]
                OutputCounters::bump(&self.test_counters.bottom_statusline_paints, 1);
                self.draw_statusline(statusline, statusline_key, cols, snapshot.scroll_indicator)?;
                self.restore_bottom_cursor()?;
                self.record_bottom_drawn(snapshot);
                return Ok(());
            }
            BottomRedrawPlan::Full => {}
        }
        #[cfg(test)]
        OutputCounters::bump(&self.test_counters.bottom_full_paints, 1);
        let trace = FrameTrace::start("bottom");
        let frame_start = self.frame_len();
        let reserve = self.statusline_reserve();
        let mut stdout = frame_out!(self);

        if let Some(prompt_rows) = self.overlay_prompt_rows(cols, rows) {
            let line_count = prompt_rows.len();
            let input_top = input_top_row(rows, reserve, line_count);
            let sep_above = input_top.saturating_sub(1);

            self.clear_shrunk_rows(self.prev_input_height, line_count)?;
            self.prev_input_height = line_count;

            if sep_above < input_top {
                self.draw_separator(sep_above, cols)?;
            }

            let prompt_color = self.color(self.palette.permission);
            for (i, line) in prompt_rows.iter().enumerate() {
                let render_row = input_top.saturating_add(i as u16);
                stdout.queue(MoveTo(0, render_row))?;
                if let Some(bg) = self.input_bg {
                    write!(stdout, "{}", SetBackgroundColor(self.color(bg)))?;
                }
                write!(stdout, "{}", SetForegroundColor(prompt_color))?;
                write!(stdout, "{}", line)?;
                write!(stdout, "{}", Clear(ClearType::UntilNewLine))?;
                write!(stdout, "{}", ResetColor)?;
            }

            let sep_below = rows.saturating_sub(reserve.saturating_sub(1));
            if sep_below < rows.saturating_sub(1) {
                self.draw_separator(sep_below, cols)?;
            }

            self.draw_statusline(statusline, statusline_key, cols, false)?;
            write!(stdout, "{}", Hide)?;
            trace.finish(self.frame_len() - frame_start);
            self.bottom_cursor = None;
            self.record_bottom_drawn(snapshot);
            return Ok(());
        }

        // Soft-wrap: every logical line becomes one or more visual rows no
        // wider than the text area (see `crate::ui::input::wrap`).
        let visible_width = input_text_width(cols);
        let layout = crate::ui::input::wrap::wrap_rows(input_line, visible_width);
        let line_count = layout.len();

        let available_rows = (rows.saturating_sub(reserve) as usize).max(1);
        // Cap the input height to roughly 30% of the area so the chat history
        // stays visible (and therefore scrollable) above a tall input instead
        // of being squeezed to nothing.
        let max_input_rows = available_rows.min((available_rows * 3 / 10).max(5));
        let need_scroll = line_count > max_input_rows;

        let raw_prompt = if is_running {
            let frame = SPINNER[self.spinner_frame as usize];
            self.spinner_frame = (self.spinner_frame + 1) % SPINNER.len() as u8;
            frame
        } else {
            "> "
        };
        let prompt = crate::ui::utils::display_prefix(raw_prompt, cols as usize);
        let prompt_width = display_width(prompt);

        let (cursor_line, cursor_display_col) =
            crate::ui::input::wrap::cursor_position(input_line, &layout, cursor_pos);

        // Vertical scroll: keep the cursor's row within the visible window so
        // pressing Up/Down can reveal rows that don't fit on screen at once.
        // Only follow the cursor when it actually moved, so mouse-wheel scrolling
        // (which leaves the cursor put) is not snapped back every frame.
        let cursor_moved = self.last_input_cursor != cursor_pos;
        self.last_input_cursor = cursor_pos;
        let first_visible = if need_scroll {
            self.input_max_vscroll = line_count - max_input_rows;
            if cursor_moved {
                if cursor_line < self.input_vscroll_offset {
                    self.input_vscroll_offset = cursor_line;
                } else if cursor_line >= self.input_vscroll_offset + max_input_rows {
                    self.input_vscroll_offset = cursor_line - max_input_rows + 1;
                }
            }
            self.input_vscroll_offset = self.input_vscroll_offset.min(self.input_max_vscroll);
            self.input_vscroll_offset
        } else {
            self.input_vscroll_offset = 0;
            self.input_max_vscroll = 0;
            0
        };

        // Clear and draw input area
        let visible_line_count = if need_scroll {
            max_input_rows
        } else {
            line_count
        };

        self.clear_shrunk_rows(self.prev_input_height, visible_line_count)?;
        self.prev_input_height = visible_line_count;

        // Thin separator line above input
        let input_top = input_top_row(rows, reserve, visible_line_count);
        let sep_above = input_top.saturating_sub(1);
        if sep_above < input_top {
            self.draw_separator(sep_above, cols)?;
        }

        // Remember the input layout so a mouse click can be mapped back to a
        // cursor position inside the input buffer.
        self.input_base_row = input_top;
        self.input_prompt_width = prompt_width;
        self.input_first_visible = first_visible;
        self.input_visible_line_count = visible_line_count;

        for (i, row) in layout
            .iter()
            .enumerate()
            .skip(first_visible)
            .take(visible_line_count)
        {
            let render_row = input_top.saturating_add((i - first_visible) as u16);
            stdout.queue(MoveTo(0, render_row))?;

            if let Some(bg) = self.input_bg {
                write!(stdout, "{}", SetBackgroundColor(self.color(bg)))?;
            }

            if i == first_visible {
                write!(
                    stdout,
                    "{}",
                    SetForegroundColor(self.color(self.palette.tool))
                )?;
                write!(stdout, "{}", prompt)?;
                write!(
                    stdout,
                    "{}",
                    SetForegroundColor(self.color(self.palette.text))
                )?;
            } else {
                write!(stdout, "{}", " ".repeat(prompt_width))?;
            }

            // A hanging space at a wrap point may overrun by one column;
            // `display_prefix` keeps the row inside the text area.
            let display =
                crate::ui::utils::display_prefix(&input_line[row.start..row.end], visible_width);
            write!(stdout, "{}", display)?;
            write!(stdout, "{}", Clear(ClearType::UntilNewLine))?;
            write!(stdout, "{}", ResetColor)?;
        }

        // Thin separator line below input
        let sep_below = rows.saturating_sub(reserve.saturating_sub(1));
        if sep_below < rows.saturating_sub(1) {
            self.draw_separator(sep_below, cols)?;
        }

        // Status line
        self.draw_statusline(statusline, statusline_key, cols, self.scroll_offset > 0)?;

        // Cursor. Clamp to the visible input rows so that when the viewport is
        // scrolled away from the cursor row, the terminal caret stays inside
        // the input box instead of spilling onto the separator or status bar.
        let cursor_render_idx = cursor_line
            .saturating_sub(first_visible)
            .min(visible_line_count.saturating_sub(1));
        let cursor_row = input_top.saturating_add(cursor_render_idx as u16);
        let cursor_x =
            (prompt_width + cursor_display_col).min(cols.saturating_sub(1) as usize) as u16;
        stdout.queue(MoveTo(cursor_x, cursor_row))?;
        write!(stdout, "{}", Show)?;
        trace.finish(self.frame_len() - frame_start);
        self.bottom_cursor = Some((cursor_x, cursor_row));
        // The draw itself settles `input_vscroll_offset` (cursor follow /
        // clamping); record the settled value so the next identical frame is
        // recognized as unchanged. `spinner_frame` deliberately keeps the
        // pre-draw value so a running spinner still differs next frame.
        let mut drawn = snapshot;
        drawn.input_vscroll_offset = self.input_vscroll_offset;
        self.record_bottom_drawn(drawn);
        Ok(())
    }

    /// Paint chat content that a throttled streaming update left undrawn,
    /// then put the caret back. Called from the running UI tick so the last
    /// tokens of a burst always appear even when no further token arrives.
    /// Returns whether anything was painted.
    pub(crate) fn paint_pending_chat(&mut self) -> io::Result<bool> {
        if !self.chat_needs_redraw() {
            return Ok(false);
        }
        self.frame(|this| {
            this.render_viewport()?;
            this.restore_bottom_cursor()
        })?;
        Ok(true)
    }

    /// Advance only the running prompt's spinner cell. The 100 ms UI tick uses
    /// this after an ordinary full draw has established the input geometry.
    pub(crate) fn tick_spinner(&mut self) -> io::Result<()> {
        self.frame(Self::tick_spinner_in_frame)
    }

    fn tick_spinner_in_frame(&mut self) -> io::Result<()> {
        let Some(snapshot) = self.last_bottom_snapshot.as_ref() else {
            return Ok(());
        };
        if self.bottom_dirty
            || !snapshot.is_running
            || !matches!(snapshot.prompt, PromptSnapshot::Input)
        {
            return Ok(());
        }

        let drawn_frame = self.spinner_frame;
        let mut stdout = frame_out!(self);
        stdout.queue(MoveTo(0, self.input_base_row))?;
        if let Some(bg) = self.input_bg {
            write!(stdout, "{}", SetBackgroundColor(self.color(bg)))?;
        }
        write!(
            stdout,
            "{}{}{}",
            SetForegroundColor(self.color(Color::DarkYellow)),
            SPINNER[drawn_frame as usize],
            ResetColor
        )?;
        self.spinner_frame = (self.spinner_frame + 1) % SPINNER.len() as u8;
        if let Some(snapshot) = self.last_bottom_snapshot.as_mut() {
            snapshot.spinner_frame = drawn_frame;
        }
        self.restore_bottom_cursor()
    }
}

fn hash_value(value: impl Hash) -> u64 {
    let mut state = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut state);
    state.finish()
}

/// Validate that a URL is safe to hand to the OS opener: an absolute http(s)
/// URL with a non-empty host, no control characters, and a sane length.
/// Literal spaces are accepted after the host because browser launch APIs
/// preserve or encode them as URL data. Anything else (file:, javascript:,
/// bare paths, ...) is rejected before it reaches an OS API.
pub(crate) fn is_safe_url(url: &str) -> bool {
    if url.is_empty() || url.len() > 2048 {
        return false;
    }
    if url
        .chars()
        .any(|c| c.is_control() || (c.is_whitespace() && c != ' '))
    {
        return false;
    }
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    !host.is_empty() && !host.chars().any(char::is_whitespace)
}

/// The complete data passed to `ShellExecuteW`. There is deliberately no
/// command line: `target` is the file/URL operand and `parameters` stays null.
/// Keeping construction platform-neutral makes this security boundary testable
/// on every host as well as in the Windows CI row.
#[cfg(any(test, windows))]
pub(crate) struct WindowsOpenRequest {
    pub(crate) verb: Vec<u16>,
    pub(crate) target: Vec<u16>,
    pub(crate) parameters: Option<Vec<u16>>,
}

#[cfg(any(test, windows))]
impl WindowsOpenRequest {
    #[cfg(test)]
    pub(crate) fn verb_text(&self) -> String {
        String::from_utf16_lossy(&self.verb[..self.verb.len().saturating_sub(1)])
    }
}

#[cfg(any(test, windows))]
pub(crate) fn windows_open_request(url: &str) -> Option<WindowsOpenRequest> {
    is_safe_url(url).then(|| WindowsOpenRequest {
        verb: "open".encode_utf16().chain([0]).collect(),
        target: url.encode_utf16().chain([0]).collect(),
        parameters: None,
    })
}

/// Execute the production Windows dispatch contract through a supplied
/// `ShellExecuteW` leaf. Tests replace only that OS call and therefore exercise
/// the same validation, operand selection, and status mapping as production.
#[cfg(any(test, windows))]
pub(crate) fn dispatch_windows_open(
    url: &str,
    shell_execute: impl FnOnce(&[u16], &[u16], Option<&[u16]>, Option<&[u16]>) -> isize,
) -> anyhow::Result<()> {
    let request = windows_open_request(url)
        .ok_or_else(|| anyhow::anyhow!("refusing to dispatch invalid Windows browser URL"))?;
    let result = shell_execute(
        &request.verb,
        &request.target,
        request.parameters.as_deref(),
        None,
    );

    if result > 32 {
        Ok(())
    } else {
        anyhow::bail!(
            "Windows browser opener failed with ShellExecuteW code {}",
            result
        )
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn shell_execute_windows(
    operation: &[u16],
    file: &[u16],
    parameters: Option<&[u16]>,
    directory: Option<&[u16]>,
) -> isize {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    if !is_nul_terminated_utf16(operation)
        || !is_nul_terminated_utf16(file)
        || parameters.is_some_and(|value| !is_nul_terminated_utf16(value))
        || directory.is_some_and(|value| !is_nul_terminated_utf16(value))
    {
        return 0;
    }
    let parameters = parameters.map_or(std::ptr::null(), <[u16]>::as_ptr);
    let directory = directory.map_or(std::ptr::null(), <[u16]>::as_ptr);
    // SAFETY: every non-null slice is an immutable, NUL-terminated UTF-16
    // allocation that remains alive for the duration of the call. Null handles
    // and null parameters/directory pointers are supported by ShellExecuteW.
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            parameters,
            directory,
            SW_SHOWNORMAL,
        ) as isize
    }
}

#[cfg(any(test, windows))]
pub(crate) fn is_nul_terminated_utf16(value: &[u16]) -> bool {
    value.last() == Some(&0) && !value[..value.len().saturating_sub(1)].contains(&0)
}

#[cfg(windows)]
fn open_url_windows(url: &str) -> anyhow::Result<()> {
    dispatch_windows_open(url, shell_execute_windows)
}

fn validate_open_url(url: &str) -> anyhow::Result<()> {
    if !is_safe_url(url) {
        let preview: String = url.chars().take(80).collect();
        anyhow::bail!("refusing to open invalid or non-http(s) URL: {}", preview);
    }

    Ok(())
}

/// Dispatch a validated URL to the desktop. A running opener may be the browser
/// itself, so successful dispatch does not imply that a page has finished opening.
pub async fn open_url(url: &str) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        validate_open_url(url)?;
        open_url_windows(url)
    }

    #[cfg(not(windows))]
    {
        let mut commands = [
            tokio::process::Command::new("xdg-open"),
            tokio::process::Command::new("open"),
        ];
        open_url_with_commands(url, &mut commands, std::time::Duration::from_secs(2)).await
    }
}

#[cfg(not(windows))]
pub(crate) async fn open_url_with_commands(
    url: &str,
    commands: &mut [tokio::process::Command],
    observation: std::time::Duration,
) -> anyhow::Result<()> {
    use crate::process_creation::TokioCommandCreationExt;

    validate_open_url(url)?;
    for command in commands {
        command
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(false);
        #[cfg(unix)]
        command.process_group(0);
        let Ok(mut child) = command.spawn_guarded() else {
            continue; // opener not installed
        };
        match tokio::time::timeout(observation, child.wait()).await {
            Ok(Ok(status)) if status.success() => return Ok(()),
            Ok(_) => continue,
            Err(_) => {
                // xdg-open can run the browser in the foreground. Do not kill
                // it or launch a duplicate fallback; reap it whenever it exits.
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
                return Ok(());
            }
        }
    }
    anyhow::bail!("no working opener found (tried xdg-open and open)")
}

pub(crate) const MAX_CLIPBOARD_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardCopyOutcome {
    Confirmed,
    FallbackRequested,
}

pub(crate) fn validate_clipboard_text(text: &str) -> anyhow::Result<()> {
    if text.contains('\0') {
        anyhow::bail!("clipboard text contains an embedded NUL");
    }
    let units = normalize_windows_clipboard_newlines(text)
        .encode_utf16()
        .count()
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("clipboard text is too large"))?;
    if units > MAX_CLIPBOARD_BYTES / std::mem::size_of::<u16>() {
        anyhow::bail!("clipboard text is too large");
    }
    Ok(())
}

pub(crate) fn normalize_windows_clipboard_newlines(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                normalized.push_str("\r\n");
            }
            '\n' => normalized.push_str("\r\n"),
            _ => normalized.push(ch),
        }
    }
    normalized
}

#[cfg(any(windows, test))]
pub(crate) fn normalize_internal_clipboard_newlines(text: String) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

pub(crate) fn osc52_request(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()))
}

fn request_terminal_clipboard(text: &str) -> anyhow::Result<ClipboardCopyOutcome> {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(osc52_request(text).as_bytes())?;
    stdout.flush()?;
    Ok(ClipboardCopyOutcome::FallbackRequested)
}

/// Copy `text` to the system clipboard. Native Windows CF_UNICODETEXT and
/// successful Unix clipboard utilities are confirmed. OSC 52 only reports
/// that a terminal fallback was requested because it has no acknowledgement.
pub async fn copy_to_clipboard(text: &str) -> anyhow::Result<ClipboardCopyOutcome> {
    validate_clipboard_text(text)?;

    #[cfg(windows)]
    {
        if windows_clipboard::write(text).is_ok() {
            return Ok(ClipboardCopyOutcome::Confirmed);
        }
        request_terminal_clipboard(text)
    }

    #[cfg(not(windows))]
    {
        let cmds: &[(&str, &[&str])] = &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("pbcopy", &[]),
        ];
        for &(cmd, args) in cmds {
            let mut command = tokio::process::Command::new(cmd);
            command.args(args);
            if run_clipboard_command(command, text, std::time::Duration::from_secs(2)).await {
                return Ok(ClipboardCopyOutcome::Confirmed);
            }
        }
        request_terminal_clipboard(text)
    }
}

#[cfg(not(windows))]
struct ClipboardProcessGroup(Option<u32>);

#[cfg(not(windows))]
impl Drop for ClipboardProcessGroup {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            crate::sandbox::kill_process_group_if_live(pid);
        }
    }
}

#[cfg(not(windows))]
pub(crate) async fn run_clipboard_command(
    mut command: tokio::process::Command,
    text: &str,
    timeout: std::time::Duration,
) -> bool {
    use crate::process_creation::TokioCommandCreationExt;
    use tokio::io::AsyncWriteExt;

    let input = text.as_bytes().to_vec();
    let (mut response, receiver) = tokio::sync::oneshot::channel();
    // The owner finishes cleanup even if the UI abandons its copy request.
    tokio::spawn(async move {
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        crate::sandbox::configure_child_lifetime(&mut command);
        let Ok(mut child) = command.spawn_guarded() else {
            let _ = response.send(false);
            return;
        };
        let mut cleanup = ClipboardProcessGroup(child.id());
        let copied = tokio::select! {
            biased;
            _ = response.closed() => false,
            result = tokio::time::timeout(timeout, async {
                let mut stdin = child.stdin.take().ok_or_else(|| {
                    std::io::Error::other("clipboard helper has no stdin")
                })?;
                stdin.write_all(&input).await?;
                stdin.shutdown().await?;
                drop(stdin);
                child.wait().await
            }) => matches!(result, Ok(Ok(status)) if status.success()),
        };
        if copied {
            cleanup.0 = None;
        }
        drop(cleanup);
        if !copied {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        // wl-copy and xclip may leave a clipboard-owner daemon after a
        // successful exit. Its lifetime belongs to the desktop session.
        let _ = response.send(copied);
    });
    receiver.await.unwrap_or(false)
}

pub fn read_from_clipboard() -> anyhow::Result<String> {
    #[cfg(windows)]
    {
        return Ok(windows_clipboard::read()?);
    }
    #[cfg(not(windows))]
    anyhow::bail!("native clipboard paste is unavailable on this platform")
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_clipboard {
    use std::io;
    use std::ptr;
    use std::thread;
    use std::time::Duration;

    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::System::Console::GetConsoleWindow;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
        OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
    };

    use super::{
        MAX_CLIPBOARD_BYTES, normalize_internal_clipboard_newlines,
        normalize_windows_clipboard_newlines, validate_clipboard_text,
    };

    const CF_UNICODETEXT: u32 = 13;
    const OPEN_ATTEMPTS: usize = 20;
    const OPEN_BACKOFF: Duration = Duration::from_millis(5);

    struct ClipboardGuard {
        open: bool,
    }

    impl ClipboardGuard {
        fn open(owner: HWND) -> io::Result<Self> {
            for attempt in 0..OPEN_ATTEMPTS {
                // SAFETY: `owner` is either null for reads/contention probes or
                // the current process's console window for writes. The open
                // state is closed by Drop.
                if unsafe { OpenClipboard(owner) } != 0 {
                    return Ok(Self { open: true });
                }
                #[cfg(test)]
                tests::notify_open_failure_for_test();
                if attempt + 1 < OPEN_ATTEMPTS {
                    thread::sleep(OPEN_BACKOFF);
                }
            }
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "clipboard is temporarily unavailable",
            ))
        }

        fn close(mut self) -> io::Result<()> {
            // SAFETY: this guard exists only after OpenClipboard succeeded.
            if unsafe { CloseClipboard() } == 0 {
                return Err(io::Error::last_os_error());
            }
            self.open = false;
            Ok(())
        }
    }

    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            if self.open {
                // SAFETY: this guard exists only after OpenClipboard succeeded.
                unsafe { CloseClipboard() };
            }
        }
    }

    struct OwnedGlobalMemory {
        handle: windows_sys::Win32::Foundation::HGLOBAL,
        transferred: bool,
    }

    impl OwnedGlobalMemory {
        fn from_utf16(units: &[u16]) -> io::Result<Self> {
            let bytes = units
                .len()
                .checked_mul(std::mem::size_of::<u16>())
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "clipboard text is too large")
                })?;
            // SAFETY: GlobalAlloc returns an owned movable allocation or null.
            let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: the allocation is live and large enough for `units`.
            let destination = unsafe { GlobalLock(handle) }.cast::<u16>();
            if destination.is_null() {
                // SAFETY: ownership has not transferred to the clipboard.
                unsafe { GlobalFree(handle) };
                return Err(io::Error::last_os_error());
            }
            // SAFETY: source and destination are valid for `units.len()` u16s
            // and do not overlap.
            unsafe { ptr::copy_nonoverlapping(units.as_ptr(), destination, units.len()) };
            // A zero return here normally means the lock count reached zero.
            // SAFETY: the handle was successfully locked above.
            unsafe { GlobalUnlock(handle) };
            Ok(Self {
                handle,
                transferred: false,
            })
        }
    }

    impl Drop for OwnedGlobalMemory {
        fn drop(&mut self) {
            if !self.transferred {
                // SAFETY: this process still owns the allocation.
                unsafe { GlobalFree(self.handle) };
            }
        }
    }

    fn write_with_owner(text: &str, owner: HWND) -> io::Result<()> {
        validate_clipboard_text(text)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid clipboard text"))?;
        let normalized = normalize_windows_clipboard_newlines(text);
        let mut units: Vec<u16> = normalized.encode_utf16().collect();
        units.push(0);
        let mut memory = OwnedGlobalMemory::from_utf16(&units)?;
        if owner.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "native clipboard copy requires an attached Windows console",
            ));
        }
        let clipboard = ClipboardGuard::open(owner)?;
        // SAFETY: this thread owns the open clipboard for both calls.
        if unsafe { EmptyClipboard() } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CF_UNICODETEXT requires movable global memory containing a
        // NUL-terminated UTF-16 string. Ownership transfers only on success.
        if unsafe { SetClipboardData(CF_UNICODETEXT, memory.handle) }.is_null() {
            return Err(io::Error::last_os_error());
        }
        memory.transferred = true;
        clipboard.close()
    }

    pub(super) fn write(text: &str) -> io::Result<()> {
        // EmptyClipboard requires a real owner before SetClipboardData. A TUI
        // attached to either ConHost or a pseudoconsole has a console HWND.
        // SAFETY: GetConsoleWindow takes no arguments and returns a borrowed
        // process-associated window handle.
        write_with_owner(text, unsafe { GetConsoleWindow() })
    }

    pub(super) fn read() -> io::Result<String> {
        let clipboard = ClipboardGuard::open(ptr::null_mut())?;
        // SAFETY: this thread owns the open clipboard.
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Unicode clipboard text is unavailable",
            ));
        }
        // SAFETY: the returned handle remains owned by the clipboard.
        let handle = unsafe { GetClipboardData(CF_UNICODETEXT) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: GetClipboardData returned a global-memory handle.
        let bytes = unsafe { GlobalSize(handle) };
        if bytes == 0 || bytes > MAX_CLIPBOARD_BYTES || bytes % std::mem::size_of::<u16>() != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "clipboard text has an invalid size",
            ));
        }
        // SAFETY: the clipboard stays open while the handle is locked/read.
        let data = unsafe { GlobalLock(handle) }.cast::<u16>();
        if data.is_null() {
            return Err(io::Error::last_os_error());
        }
        let unit_count = bytes / std::mem::size_of::<u16>();
        // SAFETY: GlobalSize bounds this slice and the allocation is locked.
        let units = unsafe { std::slice::from_raw_parts(data, unit_count) };
        let terminator = units.iter().position(|unit| *unit == 0);
        let decoded = terminator
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "clipboard text is not terminated",
                )
            })
            .and_then(|end| {
                String::from_utf16(&units[..end]).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "clipboard text is invalid UTF-16",
                    )
                })
            });
        // SAFETY: the handle was successfully locked above.
        unsafe { GlobalUnlock(handle) };
        let decoded = normalize_internal_clipboard_newlines(decoded?);
        clipboard.close()?;
        Ok(decoded)
    }

    #[cfg(test)]
    mod tests {
        use std::sync::Mutex;
        use std::sync::mpsc;

        use windows_sys::Win32::Foundation::HWND;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, HWND_MESSAGE,
        };

        use super::{ClipboardGuard, read, write_with_owner};

        static CLIPBOARD_TEST: Mutex<()> = Mutex::new(());
        static OPEN_FAILURE_HOOK: Mutex<Option<mpsc::Sender<()>>> = Mutex::new(None);

        pub(super) fn notify_open_failure_for_test() {
            if let Some(sender) = OPEN_FAILURE_HOOK.lock().unwrap().take() {
                let _ = sender.send(());
            }
        }

        struct ClipboardOwner(HWND);

        impl ClipboardOwner {
            fn new() -> Self {
                let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
                // SAFETY: STATIC is a system window class. The message-only
                // window stays live for the complete clipboard operation.
                let owner = unsafe {
                    CreateWindowExW(
                        0,
                        class.as_ptr(),
                        std::ptr::null(),
                        0,
                        0,
                        0,
                        0,
                        0,
                        HWND_MESSAGE,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null(),
                    )
                };
                assert!(
                    !owner.is_null(),
                    "test must create a clipboard owner window"
                );
                Self(owner)
            }
        }

        impl Drop for ClipboardOwner {
            fn drop(&mut self) {
                // SAFETY: this test owns the window and destroys it once.
                unsafe { DestroyWindow(self.0) };
            }
        }

        struct RestoreClipboard {
            owner: HWND,
            text: Option<String>,
        }

        impl Drop for RestoreClipboard {
            fn drop(&mut self) {
                if let Some(text) = self.text.as_deref() {
                    let _ = write_with_owner(text, self.owner);
                }
            }
        }

        fn hold_clipboard() -> (mpsc::Sender<()>, std::thread::JoinHandle<()>) {
            let (opened_tx, opened_rx) = mpsc::sync_channel(1);
            let (release_tx, release_rx) = mpsc::channel();
            let holder = std::thread::spawn(move || {
                let guard =
                    ClipboardGuard::open(std::ptr::null_mut()).expect("test must open clipboard");
                opened_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                drop(guard);
            });
            opened_rx.recv().unwrap();
            (release_tx, holder)
        }

        #[test]
        #[ignore = "mutates the process-global Windows clipboard; run in isolated Windows CI"]
        fn windows_clipboard_round_trips_unicode_empty_and_multiline_text() {
            let _serial = CLIPBOARD_TEST.lock().unwrap();
            let owner = ClipboardOwner::new();
            let _restore = RestoreClipboard {
                owner: owner.0,
                text: read().ok(),
            };
            for text in ["ASCII", "non-BMP 🧰 𐐷", "", "first\nsecond\nthird"] {
                write_with_owner(text, owner.0).unwrap();
                assert_eq!(read().unwrap(), text);
            }
        }

        #[test]
        #[ignore = "mutates the process-global Windows clipboard; run in isolated Windows CI"]
        fn windows_clipboard_retries_transient_contention_and_recovers_after_exhaustion() {
            let _serial = CLIPBOARD_TEST.lock().unwrap();
            let owner = ClipboardOwner::new();
            let _restore = RestoreClipboard {
                owner: owner.0,
                text: read().ok(),
            };

            let (release, holder) = hold_clipboard();
            let (failed_tx, failed_rx) = mpsc::channel();
            *OPEN_FAILURE_HOOK.lock().unwrap() = Some(failed_tx);
            let releaser = std::thread::spawn(move || {
                failed_rx.recv().unwrap();
                release.send(()).unwrap();
            });
            write_with_owner("transient", owner.0).unwrap();
            releaser.join().unwrap();
            holder.join().unwrap();
            assert_eq!(read().unwrap(), "transient");

            let (release, holder) = hold_clipboard();
            let error = write_with_owner("persistent", owner.0).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
            release.send(()).unwrap();
            holder.join().unwrap();

            write_with_owner("recovered", owner.0).unwrap();
            assert_eq!(read().unwrap(), "recovered");
        }
    }
}

/// Minimal base64 encoder — avoids pulling in a crate just for clipboard support.
pub(crate) fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
        let b2 = chunk.get(2).copied().unwrap_or(0) as usize;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) & 63] as char);
        out.push(ALPHABET[(triple >> 12) & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) & 63]
        } else {
            b'='
        } as char);
        out.push(if chunk.len() > 2 {
            ALPHABET[triple & 63]
        } else {
            b'='
        } as char);
    }
    out
}

#[cfg(test)]
mod captured_app_geometry_tests {
    use super::*;

    fn would_block_query() -> io::Result<(u16, u16)> {
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "controlled native terminal query failure",
        ))
    }
    fn forbidden_query() -> io::Result<(u16, u16)> {
        panic!("captured App renderer must never query the native terminal")
    }

    #[test]
    fn uncaptured_bottom_draw_preserves_native_geometry_errors() {
        let mut renderer = Renderer::new().unwrap();
        renderer.test_native_size_query = Some(would_block_query);
        let error = renderer.draw_bottom("draft", 5, &[], 0, false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(renderer.native_size_queries.get(), 1);
    }

    fn fixed_query() -> io::Result<(u16, u16)> {
        Ok((120, 40))
    }

    fn refresh_like(renderer: &mut Renderer, input: &str) {
        renderer
            .frame(|r| {
                r.sync_input_height(input)?;
                r.render_viewport()?;
                r.draw_bottom(input, input.len(), &[], 0, false)
            })
            .unwrap();
    }

    #[test]
    fn a_refresh_queries_the_native_terminal_size_at_most_once() {
        let mut renderer = Renderer::for_native_geometry_test(fixed_query).unwrap();
        renderer
            .feed_mut()
            .push_block(crate::ui::feed::BlockStyle::Agent, "chat");
        refresh_like(&mut renderer, "");
        assert!(
            renderer.native_size_queries.get() <= 2,
            "first frame fills the cache"
        );
        for draft in ["d", "dr", "dra"] {
            let before = renderer.native_size_queries.get();
            refresh_like(&mut renderer, draft);
            assert_eq!(renderer.native_size_queries.get() - before, 1);
        }
        // A resize forgets the geometry: the next refresh lays out once more.
        renderer.resize();
        let before = renderer.native_size_queries.get();
        refresh_like(&mut renderer, "dra");
        assert!(renderer.native_size_queries.get() - before <= 2);
        assert_eq!(renderer.terminal_size(), (120, 40));
    }

    #[test]
    fn actual_captured_bottom_draw_owns_geometry_and_never_queries_native_terminal() {
        let mut renderer = Renderer::for_app_test().unwrap();
        renderer.test_native_size_query = Some(forbidden_query);
        renderer.sync_input_height("owned draft").unwrap();
        renderer.render_viewport().unwrap();
        renderer
            .draw_bottom("owned draft", 11, &[], 0, false)
            .unwrap();
        assert_eq!(renderer.native_size_queries.get(), 0);
        assert!(renderer.captured_output().contains("owned draft"));
        renderer.permission_prompt = Some(PermissionPrompt {
            tool: "[permission] owned request".into(),
            options: "(n) deny once".into(),
            expanded: false,
        });
        renderer.sync_input_height("").unwrap();
        renderer.render_viewport().unwrap();
        renderer.draw_bottom("", 0, &[], 1, true).unwrap();
        renderer.resize();
        renderer.invalidate();
        renderer.draw_bottom("", 0, &[], 2, true).unwrap();
        assert_eq!(renderer.native_size_queries.get(), 0);
        assert!(renderer.captured_output().contains("owned request"));
    }
}

#[cfg(test)]
mod mutation_diff_capture_tests {
    use super::*;
    use crate::agent::tools::mutation_diff::{DiffOptions, MutationDiff};

    #[test]
    fn actual_renderer_paints_semantic_diff_and_preserves_monochrome_theme_behavior() {
        let diff =
            MutationDiff::replacement("f", Some("old🙂\n"), "newα\n", DiffOptions::default())
                .unwrap();
        for monochrome in [false, true] {
            let mut renderer = Renderer::for_app_test().unwrap();
            renderer.set_monochrome(monochrome);
            renderer.set_background_colors(
                Some(Color::Rgb {
                    r: 10,
                    g: 20,
                    b: 30,
                }),
                None,
                None,
            );
            renderer
                .feed_mut()
                .push_anchored_block("call", BlockStyle::Tool, "write f");
            crate::ui::events::push_mutation_diff(
                renderer.feed_mut(),
                "call",
                &diff,
                &crate::config::Config::default(),
            );
            renderer.render_viewport().unwrap();
            let painted = renderer.captured_output();
            assert!(
                painted.contains("+newα") && painted.contains("-old🙂") && painted.contains("@@")
            );
            assert_eq!(
                renderer.color(BlockStyle::DiffAdd.color()),
                if monochrome {
                    Color::Reset
                } else {
                    Color::Green
                }
            );
            assert_eq!(
                renderer.background_colors_for_test().0,
                Some(Color::Rgb {
                    r: 10,
                    g: 20,
                    b: 30
                })
            );
            assert_eq!(renderer.native_size_queries.get(), 0);
        }
    }
}

#[cfg(test)]
mod statusline_layout_tests {
    use super::*;
    use crate::config::types::{StatusLineConfig, StatusLineLine, StatusLineSegment};
    use crate::session::Session;
    use crate::ui::statusline::{self, StatusContext};

    #[test]
    fn light_theme_status_warnings_use_contrast_roles_and_keep_explicit_colors() {
        let renderer = &mut Renderer::for_app_test().unwrap();
        crate::context::themes::apply(
            include_str!("../../data/themes/default-light.json"),
            renderer,
        )
        .unwrap();
        let mut session = Session::new("p", "model", 1000, "");
        session.overhead_tokens = 950;
        let context = StatusContext {
            workspace: std::path::Path::new("."),
            loop_label: None,
            goal_label: None,
            prompt_name: None,
            reasoning_effort: None,
            perm_mode: None,
            chain_label: None,
            background_jobs: 0,
            todos: Some((0, 0)),
            queued_inputs: 0,
            btw_cost: 0.0,
            btw_in: 0,
            btw_out: 0,
            sandbox_label: None,
        };
        let spec = StatusLineConfig {
            lines: vec![StatusLineLine {
                segments: vec![
                    StatusLineSegment {
                        item: "context".into(),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "separator".into(),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "model".into(),
                        color: Some("yellow".into()),
                        ..Default::default()
                    },
                ],
            }],
        };
        let rows = statusline::build_lines(&spec, &session, &context);
        renderer
            .statusline_row_frame_for_test(5, &rows[0], None, 80)
            .unwrap();
        let captured = renderer.captured_output();
        let StatusSpan::Text { layout, fg, .. } = &rows[0][0] else {
            panic!("context item")
        };
        let warning = renderer.status_foreground(layout, *fg);
        assert_eq!(
            warning,
            Color::Rgb {
                r: 137,
                g: 80,
                b: 0
            }
        );
        assert!(
            captured.contains(&format!("{}", SetForegroundColor(warning))),
            "{captured:?}"
        );
        let StatusSpan::Text { layout, fg, .. } = &rows[0][2] else {
            panic!("custom model item")
        };
        assert_eq!(renderer.status_foreground(layout, *fg), Color::Yellow);
        assert!(captured.contains(&format!("{}", SetForegroundColor(Color::Yellow))));
        renderer.capture_bytes().lock().unwrap().clear();
        session.overhead_tokens = 1100;
        let rows = statusline::build_lines(&spec, &session, &context);
        renderer
            .statusline_row_frame_for_test(5, &rows[0], None, 80)
            .unwrap();
        let StatusSpan::Text { layout, fg, .. } = &rows[0][0] else {
            panic!("context item")
        };
        let error = renderer.status_foreground(layout, *fg);
        assert_eq!(
            error,
            Color::Rgb {
                r: 180,
                g: 35,
                b: 24
            }
        );
        assert!(
            renderer
                .captured_output()
                .contains(&format!("{}", SetForegroundColor(error)))
        );
    }

    #[test]
    fn actual_statusline_draw_reserves_last_cell_and_marks_unicode_clipping() {
        let mut renderer = Renderer::for_app_test().unwrap();
        let session = Session::new("p", "m", 1000, "/workspace");
        let context = StatusContext {
            workspace: std::path::Path::new("/workspace"),
            loop_label: None,
            goal_label: None,
            prompt_name: Some("界界界界界界界界"),
            reasoning_effort: None,
            perm_mode: None,
            chain_label: None,
            background_jobs: 0,
            todos: Some((0, 0)),
            queued_inputs: 0,
            btw_cost: 0.0,
            btw_in: 0,
            btw_out: 0,
            sandbox_label: None,
        };
        let spec = StatusLineConfig {
            lines: vec![StatusLineLine {
                segments: vec![StatusLineSegment {
                    item: "prompt".into(),
                    ..Default::default()
                }],
            }],
        };
        let spans = statusline::build_lines(&spec, &session, &context);
        renderer
            .statusline_row_frame_for_test(5, &spans[0], None, 12)
            .unwrap();
        let text = crate::ui::events::sanitize_output(&renderer.captured_output());
        assert_eq!(text, "prompt:界…");
        assert!(display_width(&text) <= 11);
        renderer.capture_bytes().lock().unwrap().clear();
        renderer
            .statusline_row_frame_for_test(5, &spans[0], None, 30)
            .unwrap();
        let wide = crate::ui::events::sanitize_output(&renderer.captured_output());
        assert_eq!(wide, "prompt:界界界界界界界界");
        assert!(!wide.contains('…'));
    }

    #[test]
    fn actual_statusline_draw_splits_flex_and_relayouts_on_resize_without_rebuild() {
        let mut renderer = Renderer::for_app_test().unwrap();
        let item = |text: &str| StatusSpan::Text {
            text: text.into(),
            fg: None,
            bg: None,
            layout: statusline::SpanLayout::Item {
                priority: 100,
                plain: text.into(),
                compact: None,
                dynamic_color: false,
            },
        };
        let spans = [
            item("a"),
            StatusSpan::Flex,
            item("b"),
            StatusSpan::Flex,
            item("c"),
        ];
        renderer
            .statusline_row_frame_for_test(5, &spans, None, 12)
            .unwrap();
        let text = crate::ui::events::sanitize_output(&renderer.captured_output());
        assert_eq!(text, "a    b    c");
        assert_eq!(display_width(&text), 11);
        renderer.capture_bytes().lock().unwrap().clear();
        renderer
            .statusline_row_frame_for_test(5, &spans, None, 6)
            .unwrap();
        assert_eq!(
            crate::ui::events::sanitize_output(&renderer.captured_output()),
            "a b c"
        );
    }
}

#[cfg(test)]
mod syntax_paint_tests {
    use super::*;
    use crate::ui::highlight::TokenSpan;
    #[test]
    fn syntax_row_paint_preserves_unicode_selection_and_plain_copy_text() {
        let entry = LineEntry {
            text: "let 雪=1;".into(),
            color: Color::DarkYellow,
            syntax_spans: Some(
                vec![
                    TokenSpan {
                        range: 0..3,
                        color: Color::Cyan,
                    },
                    TokenSpan {
                        range: 4..7,
                        color: Color::Green,
                    },
                    TokenSpan {
                        range: 8..9,
                        color: Color::Yellow,
                    },
                ]
                .into(),
            ),
        };
        let mut painted = Vec::new();
        paint_styled_row(&mut painted, &entry, Some((4, 7)), false).unwrap();
        let painted = String::from_utf8(painted).unwrap();
        assert!(painted.contains(&SetForegroundColor(Color::Cyan).to_string()));
        assert!(painted.contains(&format!(
            "{}雪{}",
            SetAttribute(Attribute::Reverse),
            SetAttribute(Attribute::NoReverse)
        )));
        assert_eq!(entry.text, "let 雪=1;");
        let mut mono = Vec::new();
        paint_styled_row(&mut mono, &entry, None, true).unwrap();
        assert_eq!(String::from_utf8(mono).unwrap(), "let 雪=1;");
    }
    #[test]
    fn syntax_actual_viewport_paints_theme_foregrounds_and_monochrome_keeps_plain_text() {
        let mut renderer = Renderer::for_app_test().unwrap();
        let palette = ForegroundPalette {
            link: Color::Rgb {
                r: 10,
                g: 180,
                b: 220,
            },
            diff_add: Color::Rgb {
                r: 40,
                g: 220,
                b: 150,
            },
            ..Default::default()
        };
        renderer.set_foreground_palette(palette);
        renderer.feed_mut().push_block(
            BlockStyle::Agent,
            "```rust\nlet s = \"雪\";\n```\n```diff\n-old\n+new\n```\n",
        );
        renderer.render_viewport().unwrap();
        let painted = renderer.captured_output();
        // Crossterm's process-level NO_COLOR may suppress ANSI independently of
        // the renderer's monochrome setting. Typed role assertions still apply.
        if SetForegroundColor(palette.link)
            .to_string()
            .contains("38;2;")
        {
            assert!(painted.contains("\x1b[38;2;10;180;220m"));
            assert!(painted.contains("\x1b[38;2;40;220;150m"));
            assert!(painted.contains(&SetForegroundColor(palette.diff_del).to_string()));
        }
        let rows = renderer.feed().lines(120);
        assert!((0..rows.len()).any(|i| rows[i].text.contains("雪")));
        assert!(
            (0..rows.len())
                .filter_map(|i| rows[i].syntax_spans.as_ref())
                .any(|spans| spans.iter().any(|s| s.color == palette.link))
        );
        assert!(
            (0..rows.len())
                .filter_map(|i| rows[i].syntax_spans.as_ref())
                .any(|spans| spans.iter().any(|s| s.color == palette.diff_add))
        );
        renderer.capture_bytes().lock().unwrap().clear();
        renderer.set_monochrome(true);
        renderer.render_viewport().unwrap();
        let mono = renderer.captured_output();
        assert!(!mono.contains("\x1b[38;2;10;180;220m"));
        assert!(!mono.contains("\x1b[38;2;40;220;150m"));
        assert!(mono.contains("雪"));
        let plain = renderer.feed().lines(120);
        assert!((0..plain.len()).all(|i| plain[i].syntax_spans.is_none()));
    }
    #[test]
    fn syntax_url_spans_preserve_existing_complete_osc8_links() {
        let lines = crate::ui::markdown::markdown_to_styled(
            "```rust\nlet s=https://example.test/path;\n```",
            80,
        );
        let entry = lines
            .iter()
            .find(|row| row.text.contains("https://"))
            .unwrap();
        let mut painted = Vec::new();
        paint_styled_row(&mut painted, entry, None, false).unwrap();
        let painted = String::from_utf8(painted).unwrap();
        assert!(painted.contains("\x1b]8;;https://example.test/path;\x1b\\"));
        assert_eq!(sanitize_output(&painted), entry.text);
    }
    #[test]
    fn syntax_invalid_byte_ranges_use_the_complete_plain_fallback() {
        let entry = LineEntry {
            text: "雪".into(),
            color: Color::White,
            syntax_spans: Some(
                vec![TokenSpan {
                    range: 1..2,
                    color: Color::Red,
                }]
                .into(),
            ),
        };
        let mut painted = Vec::new();
        paint_styled_row(&mut painted, &entry, None, false).unwrap();
        assert_eq!(String::from_utf8(painted).unwrap(), "雪");
    }
}

#[cfg(test)]
mod frame_cost_tests {
    use super::*;
    use crate::ui::event_handler::stream_repaint_due;
    use crate::ui::feed::BlockStyle;

    /// About `target_bytes` of markdown: headings, prose with links and
    /// fenced code.
    fn markdown_response(target_bytes: usize) -> String {
        let mut text = String::with_capacity(target_bytes + 1024);
        let mut section = 0usize;
        while text.len() < target_bytes {
            section += 1;
            text.push_str(&format!(
                "## Section {section}\n\nThis paragraph explains step {section} of the change \
                 in plain prose, with `inline code`, **bold text** and a link to \
                 https://example.com/docs/{section} so the painter sees URLs too.\n\n"
            ));
            text.push_str("```rust\n");
            for line in 0..12 {
                text.push_str(&format!(
                    "fn step_{section}_{line}(value: u64) -> u64 {{ value.wrapping_mul({line}) + {section} }}\n"
                ));
            }
            text.push_str("```\n\n");
        }
        text
    }

    /// Stream a large markdown response through the renderer the way the
    /// streaming arm does (append to the running block, repaint when
    /// `stream_repaint_due`, UI tick every tenth token) and report the
    /// frame cost. Bounds are generous; the printed numbers are the baseline
    /// recorded in docs/benchmarks/2026-10-15-tui-frame-cost.md.
    #[test]
    fn frame_cost_streaming_200kb_markdown() {
        let response = markdown_response(200 * 1024);
        let mut renderer = Renderer::for_app_test().unwrap();
        renderer.sync_input_height("").unwrap();
        renderer.render_viewport().unwrap();
        renderer.draw_bottom("", 0, &[], 0, true).unwrap();
        renderer.feed_mut().push_streaming_block(BlockStyle::Agent);
        let block = renderer.feed().block_count() - 1;
        let before = renderer.frame_stats();
        let started = std::time::Instant::now();
        let mut streamed = 0usize;
        let mut tokens = 0usize;
        let mut offset = 0;
        while offset < response.len() {
            let mut end = (offset + 48).min(response.len());
            while !response.is_char_boundary(end) {
                end += 1;
            }
            let token = &response[offset..end];
            offset = end;
            tokens += 1;
            streamed += token.len();
            renderer.feed_mut().append_to(block, token);
            if stream_repaint_due(
                streamed,
                token.contains('\n'),
                Some(std::time::Duration::ZERO),
            ) {
                renderer.render_viewport().unwrap();
            }
            if tokens % 10 == 0 {
                renderer.paint_pending_chat().unwrap();
                renderer.tick_spinner().unwrap();
            }
        }
        renderer.feed_mut().finalize_block(block);
        renderer.render_viewport().unwrap();
        renderer.draw_bottom("", 0, &[], 0, false).unwrap();
        let elapsed = started.elapsed();
        let stats = renderer.frame_stats().since(before);
        let paints = stats.viewport_paints.max(1);
        println!(
            "frame_cost: tokens={tokens} viewport_paints={} rows={} bottom_full={} \
             writes={} flushes={} bytes={} bytes_per_paint={} writes_per_paint={} wall_ms={}",
            stats.viewport_paints,
            stats.viewport_rows,
            stats.bottom_full_paints,
            stats.writes,
            stats.flushes,
            stats.bytes,
            stats.bytes / paints,
            stats.writes / paints,
            elapsed.as_millis()
        );
        assert!(stats.viewport_paints > 0);
        assert!(stats.flushes > 0);
        assert!(
            elapsed < std::time::Duration::from_secs(300),
            "streaming 200 KB took {elapsed:?}"
        );
    }

    /// One `refresh_display`-shaped frame: input height, chat, bottom.
    fn refresh_like(renderer: &mut Renderer, input: &str) {
        renderer.sync_input_height(input).unwrap();
        renderer.render_viewport().unwrap();
        renderer
            .draw_bottom(input, input.len(), &[], 0, false)
            .unwrap();
    }

    /// Typing into the input under a full chat viewport: report the cost of
    /// one keystroke frame.
    #[test]
    fn frame_cost_keystrokes_under_full_viewport() {
        let mut renderer = Renderer::for_app_test().unwrap();
        for line in 0..200 {
            renderer.feed_mut().push_block(
                BlockStyle::Agent,
                format!("chat line {line} https://example.com/{line}"),
            );
        }
        refresh_like(&mut renderer, "");
        let before = renderer.frame_stats();
        let mut input = String::new();
        for ch in "a keystroke frame".chars() {
            input.push(ch);
            refresh_like(&mut renderer, &input);
        }
        let stats = renderer.frame_stats().since(before);
        let keys = "a keystroke frame".chars().count();
        println!(
            "frame_cost_keys: keys={keys} viewport_paints={} bottom_full={} writes={} flushes={} \
             bytes={} bytes_per_key={} writes_per_key={} flushes_per_key={}",
            stats.viewport_paints,
            stats.bottom_full_paints,
            stats.writes,
            stats.flushes,
            stats.bytes,
            stats.bytes / keys,
            stats.writes / keys,
            stats.flushes / keys,
        );
        assert_eq!(stats.viewport_paints, 0, "typing must not repaint the chat");
        assert_eq!(stats.bottom_full_paints, keys);
    }
}

#[cfg(test)]
mod frame_buffer_tests {
    use super::*;
    use crate::ui::feed::BlockStyle;

    fn filled_renderer() -> Renderer {
        let mut renderer = Renderer::for_app_test().unwrap();
        for line in 0..100 {
            renderer
                .feed_mut()
                .push_block(BlockStyle::Agent, format!("row {line}"));
        }
        renderer
    }

    #[test]
    fn each_paint_entry_point_writes_one_synchronized_frame() {
        let mut renderer = filled_renderer();
        let before = renderer.frame_stats();
        renderer.render_viewport().unwrap();
        let viewport = renderer.frame_stats().since(before);
        assert_eq!((viewport.writes, viewport.flushes), (1, 1));
        assert_eq!(viewport.viewport_paints, 1);

        let before = renderer.frame_stats();
        renderer.draw_bottom("draft", 5, &[], 0, false).unwrap();
        let bottom = renderer.frame_stats().since(before);
        assert_eq!((bottom.writes, bottom.flushes), (1, 1));

        let captured = renderer.captured_output();
        assert!(captured.starts_with("\x1b[?2026h"));
        assert!(captured.ends_with("\x1b[?2026l"));
        assert_eq!(captured.matches("\x1b[?2026h").count(), 2);
        assert_eq!(captured.matches("\x1b[?2026l").count(), 2);
    }

    #[test]
    fn a_whole_refresh_is_one_frame_and_an_unchanged_one_writes_nothing() {
        let mut renderer = filled_renderer();
        let before = renderer.frame_stats();
        renderer
            .frame(|r| {
                r.sync_input_height("draft")?;
                r.render_viewport()?;
                r.draw_bottom("draft", 5, &[], 0, false)
            })
            .unwrap();
        let first = renderer.frame_stats().since(before);
        assert_eq!((first.writes, first.flushes), (1, 1));
        assert_eq!(first.viewport_paints, 1);
        assert_eq!(first.bottom_full_paints, 1);

        let before = renderer.frame_stats();
        renderer
            .frame(|r| {
                r.sync_input_height("draft")?;
                r.render_viewport()?;
                r.draw_bottom("draft", 5, &[], 0, false)
            })
            .unwrap();
        let idle = renderer.frame_stats().since(before);
        assert_eq!((idle.writes, idle.flushes, idle.bytes), (0, 0, 0));
    }

    #[test]
    fn the_ui_tick_paints_chat_spinner_and_caret_in_one_frame_each() {
        let mut renderer = filled_renderer();
        renderer.render_viewport().unwrap();
        renderer.draw_bottom("", 0, &[], 0, true).unwrap();
        renderer
            .feed_mut()
            .push_block(BlockStyle::Agent, "streamed");
        let before = renderer.frame_stats();
        assert!(renderer.paint_pending_chat().unwrap());
        renderer.tick_spinner().unwrap();
        let tick = renderer.frame_stats().since(before);
        assert_eq!((tick.writes, tick.flushes), (2, 2));
    }

    #[test]
    fn lines_written_inside_a_frame_paint_once_and_lines_outside_paint_at_once() {
        let mut renderer = filled_renderer();
        renderer.render_viewport().unwrap();
        let before = renderer.frame_stats();
        renderer.begin_frame();
        for line in 0..50 {
            renderer
                .write_line(&format!("listed {line}"), Color::White)
                .unwrap();
        }
        assert_eq!(renderer.frame_stats().since(before).flushes, 0);
        renderer.end_frame().unwrap();
        renderer.render_viewport().unwrap();
        let batched = renderer.frame_stats().since(before);
        assert_eq!(batched.viewport_paints, 1);
        assert_eq!((batched.writes, batched.flushes), (1, 1));
        assert!(renderer.captured_output().contains("listed 49"));
        assert!(!renderer.chat_needs_redraw());

        // Outside a frame a line is on screen before the call returns, so
        // it stays visible across whatever the caller awaits next.
        let before = renderer.frame_stats();
        renderer
            .write_line("waiting for you", Color::White)
            .unwrap();
        let single = renderer.frame_stats().since(before);
        assert_eq!((single.viewport_paints, single.flushes), (1, 1));
    }

    fn status_item(text: &str) -> StatusSpan {
        StatusSpan::Text {
            text: text.into(),
            fg: None,
            bg: None,
            layout: crate::ui::statusline::SpanLayout::Item {
                priority: 1,
                plain: text.into(),
                compact: None,
                dynamic_color: false,
            },
        }
    }

    #[test]
    fn cursor_only_keystrokes_reuse_the_fitted_statusline_rows() {
        let mut renderer = filled_renderer();
        renderer.set_statusline_height(2);
        let statusline = vec![
            vec![
                status_item("model"),
                StatusSpan::Flex,
                status_item("ctx 4%"),
            ],
            vec![status_item("branch main")],
        ];
        let draw = |renderer: &mut Renderer, input: &str, cursor: usize, key: u64| {
            let lines = renderer.cached_statusline(key, || statusline.clone());
            renderer
                .draw_bottom(input, cursor, &lines, key, false)
                .unwrap();
        };
        draw(&mut renderer, "draft text", 10, 7);
        let builds = renderer.statusline_builds();
        let before = renderer.frame_stats();
        for cursor in (0..10).rev() {
            draw(&mut renderer, "draft text", cursor, 7);
        }
        let moved = renderer.frame_stats().since(before);
        assert_eq!(moved.bottom_full_paints, 10);
        assert_eq!(moved.statusline_fits, 0, "rows came from the cache");
        assert_eq!(renderer.statusline_builds(), builds);
        assert!(renderer.captured_output().matches("branch main").count() >= 11);

        // New statusline content, or a scroll prefix, refits.
        let before = renderer.frame_stats();
        draw(&mut renderer, "draft text", 0, 8);
        assert_eq!(renderer.frame_stats().since(before).statusline_fits, 2);
        renderer.scroll_line_up();
        let before = renderer.frame_stats();
        draw(&mut renderer, "draft text", 0, 8);
        assert_eq!(renderer.frame_stats().since(before).statusline_fits, 1);
        assert!(renderer.captured_output().contains("-- SCROLL --"));
    }

    #[test]
    fn a_failed_frame_write_propagates_and_forces_a_full_repaint() {
        let mut renderer = filled_renderer();
        renderer.render_viewport().unwrap();
        renderer.draw_bottom("draft", 5, &[], 0, false).unwrap();
        renderer.feed_mut().push_block(BlockStyle::Agent, "lost");
        renderer.fail_next_write_for_test();
        let error = renderer.render_viewport().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(renderer.chat_needs_redraw());
        let before = renderer.frame_stats();
        renderer.draw_bottom("draft", 5, &[], 0, false).unwrap();
        renderer.render_viewport().unwrap();
        let retry = renderer.frame_stats().since(before);
        assert_eq!(retry.bottom_full_paints, 1, "bottom repainted in full");
        assert_eq!(retry.viewport_paints, 1);
        assert!(renderer.captured_output().contains("lost"));
    }
}

#[cfg(test)]
mod paint_fast_path_tests {
    use super::*;

    /// The pre-fast-path painter: every chunk through `paint_text`.
    fn reference_row(chunk: &str, selected: Option<(usize, usize)>) -> Vec<u8> {
        let mut out = Vec::new();
        match selected {
            Some((start, end)) => {
                write!(out, "{}", paint_text(&chunk[..start])).unwrap();
                write!(out, "{}", SetAttribute(Attribute::Reverse)).unwrap();
                write!(out, "{}", paint_text(&chunk[start..end])).unwrap();
                write!(out, "{}", SetAttribute(Attribute::NoReverse)).unwrap();
                write!(out, "{}", paint_text(&chunk[end..])).unwrap();
            }
            None => write!(out, "{}", paint_text(chunk)).unwrap(),
        }
        out
    }

    #[test]
    fn plain_and_url_rows_paint_identical_bytes() {
        let rows = [
            "plain row",
            "",
            "unicode 雪 ✓ row",
            "see https://example.com/a?b=c for details",
            "two http://a.example and https://b.example/x",
            "http without a scheme separator",
            "control \x1b[31mred\x07 bell",
            "ht\x07tp://formed.example after sanitizing",
            "bidi \u{202e}reversed",
        ];
        for row in rows {
            let len = row.len();
            let mut selections = vec![None, Some((0, len))];
            if let Some((mid, _)) = row.char_indices().nth(row.chars().count() / 2) {
                selections.push(Some((0, mid)));
                selections.push(Some((mid, len)));
            }
            for selected in selections {
                let mut fast = Vec::new();
                paint_row_text(&mut fast, row, selected).unwrap();
                assert_eq!(
                    fast,
                    reference_row(row, selected),
                    "row {row:?} {selected:?}"
                );
            }
        }
    }

    #[test]
    fn painting_plain_rows_never_scans_for_urls() {
        let mut renderer = Renderer::for_app_test().unwrap();
        for line in 0..1000 {
            renderer
                .feed_mut()
                .push_block(BlockStyle::Plain, format!("plain row {line} with words"));
        }
        let before = url_scans_for_test();
        renderer.render_viewport().unwrap();
        for _ in 0..20 {
            renderer.scroll_line_up();
            renderer.render_viewport().unwrap();
        }
        assert_eq!(url_scans_for_test() - before, 0);
        renderer
            .feed_mut()
            .push_block(BlockStyle::Plain, "now https://example.com/linked");
        renderer.render_viewport().unwrap();
        renderer.scroll_to_bottom().unwrap();
        renderer.invalidate();
        renderer.render_viewport().unwrap();
        assert!(url_scans_for_test() > before);
        assert!(
            renderer
                .captured_output()
                .contains("\x1b]8;;https://example.com/linked\x1b\\")
        );
    }
}
