use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::LazyLock;

use compact_str::CompactString;
use crossterm::ExecutableCommand;
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::style::{
    Attribute, Color, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{Clear, ClearType};
use regex::Regex;
use smallvec::SmallVec;

use super::feed::{BlockStyle, Feed, FeedLines, style_from_color};
use super::markdown::word_wrap;
use super::statusline::StatusSpan;
use super::utils::{char_display_width, display_width, resolve_color};

static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://[^\x00-\x1f\x7f\s<>]+").expect("compile URL regex"));

fn wrap_urls_osc8(text: &str) -> String {
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

#[derive(Clone, Debug)]
pub struct LineEntry {
    pub text: CompactString,
    pub color: Color,
}

pub struct PermissionPrompt {
    pub tool: CompactString,
    pub options: CompactString,
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
    partial: CompactString,
    partial_style: BlockStyle,
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
    let header = crate::ui::events::sanitize_output(header);
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

pub struct Renderer {
    spinner_frame: u8,
    feed: Feed,
    seen_feed_retention_generation: u64,
    partial: CompactString,
    partial_style: BlockStyle,
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
    chat_bg: Option<Color>,
    input_bg: Option<Color>,
    status_bg: Option<Color>,
    pub selection_active: bool,
    pub selection_start: Option<SelectionPoint>,
    pub selection_end: Option<SelectionPoint>,
    /// Set once the pointer moved while the button was held. A plain click
    /// (press + release without movement) never copies.
    pub selection_dragged: bool,
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
}

/// A status-line message that disappears on its own.
struct Notice {
    text: CompactString,
    color: Color,
    until: std::time::Instant,
}

/// How long a transient notice stays on the status line.
pub(crate) const NOTICE_DURATION: std::time::Duration = std::time::Duration::from_secs(3);

impl Renderer {
    pub fn new() -> io::Result<Self> {
        Ok(Renderer {
            spinner_frame: 0,
            feed: Feed::new(),
            seen_feed_retention_generation: 0,
            partial: CompactString::new(""),
            partial_style: BlockStyle::Plain,
            scroll_offset: 0,
            input_vscroll_offset: 0,
            input_max_vscroll: 0,
            last_input_cursor: 0,
            input_base_row: 0,
            input_prompt_width: 0,
            input_first_visible: 0,
            input_visible_line_count: 0,
            monochrome: false,
            chat_bg: None,
            input_bg: None,
            status_bg: None,
            selection_active: false,
            selection_start: None,
            selection_end: None,
            selection_dragged: false,
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
            #[cfg(test)]
            statusline_builds: 0,
            bottom_cursor: None,
            notice: None,
            title_status: false,
            title_activity: None,
        })
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

    #[cfg(test)]
    pub(crate) fn statusline_builds(&self) -> usize {
        self.statusline_builds
    }

    /// Enable activity reporting through the terminal title.
    pub fn set_title_status(&mut self, enabled: bool) {
        self.title_status = enabled;
    }

    /// Announce `activity` in the terminal title when it changed. A no-op
    /// unless title reporting is enabled.
    pub(crate) fn set_activity(
        &mut self,
        activity: crate::ui::terminal::AgentActivity,
    ) -> io::Result<()> {
        if !self.title_status || self.title_activity == Some(activity) {
            return Ok(());
        }
        let mut stdout = io::stdout();
        stdout.write_all(crate::ui::terminal::title_sequence(activity).as_bytes())?;
        stdout.flush()?;
        self.title_activity = Some(activity);
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
        self.monochrome = monochrome;
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
    }

    fn color(&self, color: Color) -> Color {
        resolve_color(color, self.monochrome)
    }

    fn terminal_size(&self) -> (u16, u16) {
        crossterm::terminal::size().unwrap_or((80, 24))
    }

    fn max_line_width(&self) -> usize {
        let (cols, _) = self.terminal_size();
        cols.saturating_sub(1 + self.chat_margin) as usize
    }

    #[cfg(test)]
    pub fn line_width(&self) -> usize {
        self.max_line_width()
    }

    fn chat_lines(&self, width: usize) -> Arc<FeedLines> {
        let lines = self.feed.lines(width);
        if !self.partial.is_empty() {
            let color = self.partial_style.color();
            let mut partial = Vec::new();
            for chunk in word_wrap(&self.partial, width) {
                partial.push(LineEntry { text: chunk, color });
            }
            return Arc::new(lines.with_segment(Arc::new(partial)));
        }
        lines
    }

    pub fn buffer_len(&self) -> usize {
        self.chat_lines(self.max_line_width()).len()
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
            partial: self.partial.clone(),
            partial_style: self.partial_style,
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
    pub(crate) fn forget_title(&mut self) {
        self.title_activity = None;
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
        let (header, options) = if let Some(pp) = &self.permission_prompt {
            (pp.tool.as_str(), pp.options.as_str())
        } else if let Some(cp) = &self.chain_prompt {
            (cp.question.as_str(), self.chain_options())
        } else {
            return None;
        };
        let available = rows.saturating_sub(self.statusline_reserve()) as usize;
        // One column short of the edge: a full-width row leaves the terminal
        // in its pending-wrap state, where clearing to end of line misbehaves.
        let width = (cols as usize).saturating_sub(1).max(1);
        Some(prompt_block_rows(
            header,
            options,
            width,
            prompt_max_rows(available),
        ))
    }

    /// Recompute the input height and reconcile `prev_input_height` before the
    /// chat viewport is drawn, so the chat is sized against the height the input
    /// is about to use. Without this, a height change (e.g. clearing or pasting
    /// text) leaves the viewport drawn for the old size until the next redraw.
    pub fn sync_input_height(&mut self, input_line: &str) -> io::Result<()> {
        let (_, rows) = self.terminal_size();
        let new_height = self.input_visible_height(input_line, rows);
        self.clear_shrunk_rows(self.prev_input_height, new_height)?;
        self.prev_input_height = new_height;
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

    fn commit_partial(&mut self) {
        if !self.partial.is_empty() {
            self.feed
                .push_block(self.partial_style, self.partial.as_str());
            self.partial.clear();
            self.chat_dirty = true;
        }
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
        self.commit_partial();
        self.render_viewport()
    }

    pub fn render_viewport(&mut self) -> io::Result<()> {
        self.reconcile_feed_retention();
        if !self.chat_needs_redraw() {
            return Ok(());
        }
        let (cols, _rows) = self.terminal_size();
        let max_width = cols.saturating_sub(1 + self.chat_margin) as usize;
        let visible = self.visible_lines();
        let buffer = self.chat_lines(max_width);
        let total = buffer.len();
        // The viewport may have grown since the offset was set (the input box
        // shrank, the terminal was resized); keep the offset inside the
        // scrollable range so the arithmetic below never underflows.
        self.scroll_offset = clamp_scroll_offset(self.scroll_offset, total, visible);
        let mut stdout = io::stdout();
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
                stdout.execute(MoveTo(0, visual_row))?;
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

            stdout.execute(MoveTo(0, visual_row))?;

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
            match selected {
                Some((start, end)) => {
                    write!(stdout, "{}", wrap_urls_osc8(&chunk[..start]))?;
                    write!(stdout, "{}", SetAttribute(Attribute::Reverse))?;
                    write!(stdout, "{}", wrap_urls_osc8(&chunk[start..end]))?;
                    write!(stdout, "{}", SetAttribute(Attribute::NoReverse))?;
                    write!(stdout, "{}", wrap_urls_osc8(&chunk[end..]))?;
                }
                None => write!(stdout, "{}", wrap_urls_osc8(chunk))?,
            }
            write!(stdout, "{}", Clear(ClearType::UntilNewLine))?;
            write!(stdout, "{}", ResetColor)?;

            visual_row += 1;
            buf_idx += 1;
        }

        while (visual_row as usize) < visible {
            stdout.execute(MoveTo(0, visual_row))?;
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
            stdout.execute(MoveTo(x, 0))?;
            if let Some(bg) = self.chat_bg {
                write!(stdout, "{}", SetBackgroundColor(self.color(bg)))?;
            }
            write!(stdout, "{}", SetForegroundColor(self.color(color)))?;
            write!(stdout, "{}", text)?;
            write!(stdout, "{}", ResetColor)?;
        }

        stdout.flush()?;
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
            self.chat_dirty = true;
        }
    }

    pub fn write_line(&mut self, text: &str, color: Color) -> io::Result<()> {
        self.commit_partial();
        let style = style_from_color(color);
        self.feed.push_block(style, text);
        self.chat_dirty = true;
        if self.scroll_offset == 0 {
            self.render_viewport()?;
        }
        Ok(())
    }

    /// Like [`Renderer::write_line`], and later lines can be placed under
    /// this one with [`Renderer::write_line_after`] using the same `anchor`.
    pub fn write_line_anchored(
        &mut self,
        anchor: &str,
        text: &str,
        color: Color,
    ) -> io::Result<()> {
        self.commit_partial();
        self.feed
            .push_anchored_block(anchor, style_from_color(color), text);
        self.chat_dirty = true;
        if self.scroll_offset == 0 {
            self.render_viewport()?;
        }
        Ok(())
    }

    /// Place a line directly under the line written with `anchor` (a tool
    /// result under its own call), or at the end when that line is gone.
    pub fn write_line_after(&mut self, anchor: &str, text: &str, color: Color) -> io::Result<()> {
        self.commit_partial();
        let style = style_from_color(color);
        if !self.feed.insert_after_anchor(anchor, style, text) {
            self.feed.push_block(style, text);
        }
        self.chat_dirty = true;
        if self.scroll_offset == 0 {
            self.render_viewport()?;
        }
        Ok(())
    }

    pub fn write(&mut self, text: &str, color: Color) -> io::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        let style = style_from_color(color);
        let parts: SmallVec<[&str; 4]> = text.split('\n').collect();
        let last = parts.len() - 1;
        for (i, segment) in parts.iter().enumerate() {
            if i < last {
                // Complete line segment: finalize any partial and push it.
                if !self.partial.is_empty() {
                    self.commit_partial();
                }
                self.feed.push_block(style, *segment);
            } else {
                // Last segment may still be incomplete; accumulate in partial.
                self.partial_style = style;
                self.partial.push_str(segment);
            }
        }
        self.chat_dirty = true;
        if self.scroll_offset == 0 {
            self.render_viewport()?;
        }
        Ok(())
    }

    pub fn clear_content(&mut self) -> io::Result<()> {
        self.chat_dirty = true;
        self.feed.clear();
        self.partial.clear();
        self.scroll_offset = 0;
        self.clear_selection();
        let mut stdout = io::stdout();
        if let Some(bg) = self.chat_bg {
            write!(stdout, "{}", SetBackgroundColor(self.color(bg)))?;
        }
        stdout.execute(Clear(ClearType::All))?;
        write!(stdout, "{}", ResetColor)?;
        stdout.execute(MoveTo(0, 0))?;
        stdout.flush()?;
        Ok(())
    }

    pub fn resize(&mut self) {
        self.chat_dirty = true;
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
        let mut stdout = io::stdout();
        for row in old_start..new_start {
            stdout.execute(MoveTo(0, row))?;
            if let Some(bg) = self.input_bg {
                write!(stdout, "{}", SetBackgroundColor(self.color(bg)))?;
            }
            write!(stdout, "{}", Clear(ClearType::UntilNewLine))?;
            write!(stdout, "{}", ResetColor)?;
        }
        Ok(())
    }

    fn draw_separator(&self, row: u16, cols: u16) -> io::Result<()> {
        let mut stdout = io::stdout();
        stdout.execute(MoveTo(0, row))?;
        if let Some(bg) = self.input_bg {
            write!(stdout, "{}", SetBackgroundColor(self.color(bg)))?;
        }
        write!(
            stdout,
            "{}",
            SetForegroundColor(self.color(Color::DarkGrey))
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
        cols: u16,
        is_scrolling: bool,
    ) -> io::Result<()> {
        let (_, rows) = self.terminal_size();
        let h = self.statusline_height as u16;
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
            self.draw_statusline_row(
                screen_row,
                spans,
                prefix.as_ref().map(|(text, color)| (text.as_str(), *color)),
                cols,
            )?;
        }
        Ok(())
    }

    fn draw_statusline_row(
        &self,
        screen_row: u16,
        spans: &[StatusSpan],
        prefix: Option<(&str, Color)>,
        cols: u16,
    ) -> io::Result<()> {
        let mut stdout = io::stdout();
        stdout.execute(MoveTo(0, screen_row))?;
        if let Some(bg) = self.status_bg {
            write!(stdout, "{}", SetBackgroundColor(self.color(bg)))?;
        }
        write!(stdout, "{}", Clear(ClearType::CurrentLine))?;
        stdout.execute(MoveTo(0, screen_row))?;
        if let Some(bg) = self.status_bg {
            write!(stdout, "{}", SetBackgroundColor(self.color(bg)))?;
        }

        let total = cols as usize;
        let mut budget = total;

        if let Some((prefix, color)) = prefix {
            write!(stdout, "{}", SetForegroundColor(self.color(color)))?;
            let take = crate::ui::utils::display_prefix(prefix, budget);
            budget -= display_width(take);
            write!(stdout, "{}", take)?;
        }

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
        let mut flex_left = budget.saturating_sub(fixed);
        let mut flex_seen = 0usize;

        for span in spans {
            if budget == 0 {
                break;
            }
            match span {
                StatusSpan::Text { text, fg, bg } => {
                    let bgc = bg.or(self.status_bg);
                    if let Some(c) = bgc {
                        write!(stdout, "{}", SetBackgroundColor(self.color(c)))?;
                    }
                    let fgc = fg.unwrap_or(Color::DarkGrey);
                    write!(stdout, "{}", SetForegroundColor(self.color(fgc)))?;
                    let piece = crate::ui::utils::display_prefix(text, budget);
                    budget = budget.saturating_sub(display_width(piece));
                    write!(stdout, "{}", piece)?;
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
                    flex_left = flex_left.saturating_sub(width);
                    budget = budget.saturating_sub(width);
                    write!(stdout, "{}", " ".repeat(width))?;
                }
            }
        }

        write!(stdout, "{}", Clear(ClearType::UntilNewLine))?;
        write!(stdout, "{}", ResetColor)?;
        Ok(())
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
    pub(crate) fn restore_bottom_cursor(&self) -> io::Result<()> {
        let mut stdout = io::stdout();
        match self.bottom_cursor {
            Some((x, row)) => {
                stdout.execute(MoveTo(x, row))?;
                write!(stdout, "{}", Show)?;
            }
            None => {
                write!(stdout, "{}", Hide)?;
            }
        }
        stdout.flush()
    }

    pub fn draw_bottom(
        &mut self,
        input_line: &str,
        cursor_pos: usize,
        statusline: &[Vec<StatusSpan>],
        statusline_key: u64,
        is_running: bool,
    ) -> io::Result<()> {
        let (cols, rows) = crossterm::terminal::size()?;
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
                self.draw_statusline(statusline, cols, snapshot.scroll_indicator)?;
                self.restore_bottom_cursor()?;
                self.record_bottom_drawn(snapshot);
                return Ok(());
            }
            BottomRedrawPlan::Full => {}
        }
        let reserve = self.statusline_reserve();
        let mut stdout = io::stdout();

        if let Some(prompt_rows) = self.overlay_prompt_rows(cols, rows) {
            let line_count = prompt_rows.len();
            let input_top = input_top_row(rows, reserve, line_count);
            let sep_above = input_top.saturating_sub(1);

            self.clear_shrunk_rows(self.prev_input_height, line_count)?;
            self.prev_input_height = line_count;

            if sep_above < input_top {
                self.draw_separator(sep_above, cols)?;
            }

            let prompt_color = self.color(Color::DarkYellow);
            for (i, line) in prompt_rows.iter().enumerate() {
                let render_row = input_top.saturating_add(i as u16);
                stdout.execute(MoveTo(0, render_row))?;
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

            self.draw_statusline(statusline, cols, false)?;
            write!(stdout, "{}", Hide)?;
            stdout.flush()?;
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
            stdout.execute(MoveTo(0, render_row))?;

            if let Some(bg) = self.input_bg {
                write!(stdout, "{}", SetBackgroundColor(self.color(bg)))?;
            }

            if i == first_visible {
                write!(
                    stdout,
                    "{}",
                    SetForegroundColor(self.color(Color::DarkYellow))
                )?;
                write!(stdout, "{}", prompt)?;
                write!(stdout, "{}", SetForegroundColor(Color::Reset))?;
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
        self.draw_statusline(statusline, cols, self.scroll_offset > 0)?;

        // Cursor. Clamp to the visible input rows so that when the viewport is
        // scrolled away from the cursor row, the terminal caret stays inside
        // the input box instead of spilling onto the separator or status bar.
        let cursor_render_idx = cursor_line
            .saturating_sub(first_visible)
            .min(visible_line_count.saturating_sub(1));
        let cursor_row = input_top.saturating_add(cursor_render_idx as u16);
        let cursor_x =
            (prompt_width + cursor_display_col).min(cols.saturating_sub(1) as usize) as u16;
        stdout.execute(MoveTo(cursor_x, cursor_row))?;
        write!(stdout, "{}", Show)?;
        stdout.flush()?;
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
        self.render_viewport()?;
        self.restore_bottom_cursor()?;
        Ok(true)
    }

    /// Advance only the running prompt's spinner cell. The 100 ms UI tick uses
    /// this after an ordinary full draw has established the input geometry.
    pub(crate) fn tick_spinner(&mut self) -> io::Result<()> {
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
        let mut stdout = io::stdout();
        stdout.execute(MoveTo(0, self.input_base_row))?;
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
        stdout.flush()?;
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
