use std::cell::RefCell;
use std::collections::VecDeque;
use std::ops::Index;
use std::sync::Arc;

use compact_str::CompactString;
use crossterm::style::Color;

use super::markdown::{markdown_to_styled, word_wrap};
use super::renderer::LineEntry;
use super::{C_AGENT, C_ERROR, C_PERM, C_TOOL};

const MAX_FEED_BLOCKS: usize = 4_096;
const MAX_FEED_BYTES: usize = 16 * 1024 * 1024;
const MAX_BLOCK_BYTES: usize = 2 * 1024 * 1024;
const RETAINED_BLOCK_BYTES: usize = MAX_BLOCK_BYTES / 2;
const OMITTED_PREFIX: &str = "[earlier feed content omitted]\n\n";

/// Semantic role of a conversation block in the feed.
///
/// Roles are independent of terminal colors; `BlockStyle::color()` maps each
/// role to the color used by the custom renderer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockStyle {
    User,
    Agent,
    Reasoning,
    Tool,
    ToolResult,
    Error,
    System,
    Welcome,
    Permission,
    Plain,
}

impl BlockStyle {
    pub fn color(self) -> Color {
        match self {
            BlockStyle::User => Color::Green,
            BlockStyle::Agent => C_AGENT,
            BlockStyle::Reasoning => Color::DarkMagenta,
            BlockStyle::Tool => C_TOOL,
            BlockStyle::ToolResult => Color::DarkGrey,
            BlockStyle::Error => C_ERROR,
            BlockStyle::System => Color::DarkGrey,
            BlockStyle::Welcome => Color::Cyan,
            BlockStyle::Permission => C_PERM,
            BlockStyle::Plain => Color::White,
        }
    }
}

/// Map a legacy terminal color to the closest semantic block style.
///
/// This is used while migrating callers from `Renderer::write_line(text, color)`
/// to the feed model. New code should prefer `BlockStyle` directly.
pub fn style_from_color(color: Color) -> BlockStyle {
    match color {
        Color::Green => BlockStyle::User,
        Color::DarkMagenta => BlockStyle::Reasoning,
        Color::Yellow => BlockStyle::Tool,
        Color::DarkGrey => BlockStyle::System,
        Color::Cyan => BlockStyle::Welcome,
        Color::Red => BlockStyle::Error,
        Color::Magenta => BlockStyle::Permission,
        Color::White => BlockStyle::Plain,
        _ => BlockStyle::Plain,
    }
}

/// A single structured conversation block.
///
/// Blocks store raw text; layout (word-wrap, markdown parsing) happens when
/// `Feed::lines(width)` is called. This keeps the feed independent of terminal
/// geometry and makes layout math testable without a terminal.
#[derive(Clone, Debug)]
pub struct Block {
    pub style: BlockStyle,
    pub text: String,
    /// True while a producer is still appending to this block (e.g. streaming
    /// agent tokens). A running agent block parses markdown only for its
    /// completed lines and renders the unfinished tail line as plain text.
    running: bool,
    /// Memoized markdown layout. Interior mutability keeps `Feed::lines` a
    /// `&self` read; `Feed` mutators that rewrite block text invalidate it.
    md_cache: RefCell<Option<MdCache>>,
    revision: u64,
    render_cache: RefCell<Option<BlockRenderCache>>,
}

#[derive(Clone, Debug)]
struct BlockRenderCache {
    width: usize,
    revision: u64,
    segments: Vec<Arc<Vec<LineEntry>>>,
}

/// Memoized markdown layout of an agent block's completed text at a width.
///
/// Tracks both the full parse and a "stable boundary" where markdown is
/// definitely finalized (e.g., after a blank line). This allows incremental
/// parsing: when completed_len extends beyond the stable boundary but within
/// the same stable region, we can reuse the stable lines and only re-parse
/// from stable_len to completed_len.
#[derive(Clone, Debug)]
struct MdCache {
    width: usize,
    /// Byte offset where markdown is definitely finalized: after a top-level
    /// blank line, or after any completed line inside a replayable fence.
    stable_len: usize,
    /// The fence open at `stable_len`, replayed when the suffix after it is
    /// parsed on its own.
    stable_fence: Option<OpenFence>,
    /// Row segments covering text[0..stable_len]. Extending the stable prefix
    /// pushes another shared segment instead of cloning the completed rows, so
    /// a streaming chunk never copies the whole prefix.
    stable_segments: Vec<Arc<Vec<LineEntry>>>,
    /// Byte length of the parsed prefix: up to the last completed line for
    /// running blocks, the full text once finalized.
    parsed_len: usize,
    /// Rows for text[stable_len..parsed_len].
    suffix: Arc<Vec<LineEntry>>,
}

impl MdCache {
    fn has_rows(&self) -> bool {
        self.stable_segments
            .iter()
            .any(|segment| !segment.is_empty())
    }

    fn segments(&self) -> Vec<Arc<Vec<LineEntry>>> {
        let mut segments: Vec<Arc<Vec<LineEntry>>> = self
            .stable_segments
            .iter()
            .filter(|segment| !segment.is_empty())
            .map(Arc::clone)
            .collect();
        if !self.suffix.is_empty() {
            segments.push(Arc::clone(&self.suffix));
        }
        segments
    }
}

/// A fenced code block that is still open at a stable boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
struct OpenFence {
    /// The exact opening delimiter line. Replaying it in front of a suffix
    /// reproduces the parser state the suffix was written in.
    opener: CompactString,
    character: u8,
    length: usize,
    /// Whether the fence opened at top level. A fence inside a list or an
    /// indented block cannot be reproduced from its opening line alone, so it
    /// publishes no stable boundary.
    replayable: bool,
}

impl Block {
    pub fn new(style: BlockStyle, text: impl Into<String>) -> Self {
        let mut text = text.into();
        compact_oversized_block(&mut text);
        Self {
            style,
            text,
            running: false,
            md_cache: RefCell::new(None),
            revision: 0,
            render_cache: RefCell::new(None),
        }
    }
}

/// Immutable, segmented visual rows. Indexing stays O(log blocks), while a
/// feed mutation shares every unchanged block's row allocation.
#[derive(Clone, Debug, Default)]
pub struct FeedLines {
    segments: Vec<Arc<Vec<LineEntry>>>,
    starts: Vec<usize>,
    len: usize,
}

impl FeedLines {
    fn new(segments: Vec<Arc<Vec<LineEntry>>>) -> Self {
        let mut starts = Vec::with_capacity(segments.len());
        let mut len = 0usize;
        for segment in &segments {
            starts.push(len);
            len = len.saturating_add(segment.len());
        }
        Self {
            segments,
            starts,
            len,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn get(&self, index: usize) -> Option<&LineEntry> {
        if index >= self.len {
            return None;
        }
        let segment = self.starts.partition_point(|start| *start <= index) - 1;
        self.segments[segment].get(index - self.starts[segment])
    }

    pub(crate) fn with_segment(&self, segment: Arc<Vec<LineEntry>>) -> Self {
        let mut segments = self.segments.clone();
        segments.push(segment);
        Self::new(segments)
    }

    #[cfg(test)]
    pub(crate) fn segment_ptrs_for_test(&self) -> Vec<*const Vec<LineEntry>> {
        self.segments.iter().map(Arc::as_ptr).collect()
    }
}

impl Index<usize> for FeedLines {
    type Output = LineEntry;

    fn index(&self, index: usize) -> &Self::Output {
        self.get(index).expect("feed line index out of bounds")
    }
}

/// Conversation feed: a sequence of semantic blocks that can be laid out at
/// any width.
#[derive(Clone, Debug, Default)]
pub struct Feed {
    blocks: VecDeque<Block>,
    total_bytes: usize,
    pruned_blocks: u64,
    retention_generation: u64,
    /// Bumped by every content mutation. The renderer compares generations to
    /// know whether the chat viewport needs a redraw, which also catches
    /// mutations made through `Renderer::feed_mut()`.
    generation: u64,
    /// Pre-wrapped visual rows for the last requested width. Scroll and
    /// selection queries reuse these rows instead of re-laying out the whole
    /// feed each time; invalidated by any content mutation (generation bump)
    /// or a width change.
    layout_cache: RefCell<Option<LayoutCache>>,
    /// Number of full layout passes; test-only proof that queries reuse the
    /// pre-wrapped rows.
    #[cfg(test)]
    layout_computes: std::cell::Cell<usize>,
    /// Total bytes sent to markdown_to_styled during agent_block_lines calls;
    /// test-only proof that streaming achieves sub-quadratic parsing.
    #[cfg(test)]
    markdown_bytes_parsed: std::cell::Cell<usize>,
    /// Number of per-block render cache misses; proves streaming invalidates
    /// only the block receiving tokens.
    #[cfg(test)]
    block_renders: std::cell::Cell<usize>,
}

/// Memoized layout of the whole feed at a viewport width and generation.
#[derive(Clone, Debug)]
struct LayoutCache {
    width: usize,
    generation: u64,
    lines: Arc<FeedLines>,
}

impl Feed {
    pub fn new() -> Self {
        Self {
            blocks: VecDeque::new(),
            total_bytes: 0,
            pruned_blocks: 0,
            retention_generation: 0,
            generation: 0,
            layout_cache: RefCell::new(None),
            #[cfg(test)]
            layout_computes: std::cell::Cell::new(0),
            #[cfg(test)]
            markdown_bytes_parsed: std::cell::Cell::new(0),
            #[cfg(test)]
            block_renders: std::cell::Cell::new(0),
        }
    }

    /// Monotonic counter bumped on every content mutation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn clear(&mut self) {
        self.generation += 1;
        self.blocks.clear();
        self.total_bytes = 0;
        self.pruned_blocks = 0;
        self.retention_generation = self.retention_generation.wrapping_add(1);
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    pub(crate) fn retention_generation(&self) -> u64 {
        self.retention_generation
    }

    pub fn push_block(&mut self, style: BlockStyle, text: impl Into<String>) {
        self.generation += 1;
        let block = Block::new(style, text);
        self.total_bytes = self.total_bytes.saturating_add(block.text.len());
        self.blocks.push_back(block);
        self.prune_completed_prefix();
    }

    /// Push an empty block that a producer will append to incrementally
    /// (e.g. streaming agent tokens). While running, agent blocks parse
    /// markdown only for completed lines and render the unfinished tail line
    /// as plain text. Call `finalize_block` (or `finalize_last`) when the
    /// stream ends.
    pub fn push_streaming_block(&mut self, style: BlockStyle) {
        self.prune_completed_prefix();
        self.generation += 1;
        let mut block = Block::new(style, "");
        block.running = true;
        self.blocks.push_back(block);
    }

    /// Mark the last block as complete: its full text (including the former
    /// tail line) is parsed as markdown on the next layout. No-op when the
    /// last block is not running.
    #[cfg(test)]
    pub fn finalize_last(&mut self) {
        if let Some(idx) = self.blocks.len().checked_sub(1) {
            self.finalize_block(idx);
        }
    }

    /// Mark the block at `idx` as complete (see `finalize_last`). Streaming
    /// producers track their own block index because other producers (`/btw`
    /// answers, queued-input notices) may push blocks after it mid-stream.
    /// No-op when `idx` is out of range or the block is not running.
    pub fn finalize_block(&mut self, idx: usize) {
        if let Some(block) = self.blocks.get_mut(idx)
            && block.running
        {
            self.generation += 1;
            block.running = false;
            // Force one full re-parse now that the text is complete.
            *block.md_cache.borrow_mut() = None;
            *block.render_cache.borrow_mut() = None;
        }
        self.prune_completed_prefix();
    }

    /// True when the block at `idx` exists and is still being streamed into.
    pub fn is_streaming(&self, idx: usize) -> bool {
        self.blocks.get(idx).is_some_and(|block| block.running)
    }

    /// Text of the block at `idx`, if it exists.
    #[cfg(test)]
    pub fn block_text(&self, idx: usize) -> Option<&str> {
        self.blocks.get(idx).map(|block| block.text.as_str())
    }

    pub fn push_line(&mut self, style: BlockStyle, text: impl Into<String>) {
        self.push_block(style, text);
    }

    /// Append text to the most recent block. Returns `false` when the feed is
    /// empty and there is no block to append to.
    #[cfg(test)]
    pub fn append_to_last(&mut self, text: impl AsRef<str>) -> bool {
        match self.blocks.len().checked_sub(1) {
            Some(idx) => self.append_to(idx, text),
            None => false,
        }
    }

    /// Append text to the block at `idx`. Returns `false` when no such block
    /// exists. Streaming callers use this with their tracked block index so
    /// tokens never land on a block another producer pushed after theirs.
    pub fn append_to(&mut self, idx: usize, text: impl AsRef<str>) -> bool {
        if let Some(block) = self.blocks.get_mut(idx) {
            self.generation += 1;
            let old_len = block.text.len();
            block.text.push_str(text.as_ref());
            let compacted = compact_oversized_block(&mut block.text);
            self.total_bytes = self
                .total_bytes
                .saturating_sub(old_len)
                .saturating_add(block.text.len());
            block.revision = block.revision.wrapping_add(1);
            *block.render_cache.borrow_mut() = None;
            if compacted {
                *block.md_cache.borrow_mut() = None;
                self.retention_generation = self.retention_generation.wrapping_add(1);
            }
            self.prune_completed_prefix();
            true
        } else {
            false
        }
    }

    /// Replace the last block, or push a new one if the feed is empty.
    #[cfg(test)]
    pub fn replace_last(&mut self, style: BlockStyle, text: impl Into<String>) {
        self.generation += 1;
        if let Some(last) = self.blocks.back_mut() {
            self.total_bytes = self.total_bytes.saturating_sub(last.text.len());
            last.style = style;
            last.text = text.into();
            compact_oversized_block(&mut last.text);
            self.total_bytes = self.total_bytes.saturating_add(last.text.len());
            last.running = false;
            last.revision = last.revision.wrapping_add(1);
            *last.md_cache.borrow_mut() = None;
            *last.render_cache.borrow_mut() = None;
        } else {
            let block = Block::new(style, text);
            self.total_bytes = block.text.len();
            self.blocks.push_back(block);
        }
        self.prune_completed_prefix();
    }

    #[cfg(test)]
    pub fn truncate_blocks(&mut self, len: usize) {
        self.generation += 1;
        self.total_bytes = self
            .blocks
            .iter()
            .take(len)
            .map(|block| block.text.len())
            .sum();
        self.blocks.truncate(len);
    }

    /// Return the fully laid-out chat lines for the given width.
    ///
    /// The result is a list of `LineEntry` values, one per visible row, that the
    /// renderer can draw directly. Markdown is parsed for agent blocks; all
    /// other blocks are word-wrapped and colored by their semantic role.
    /// Running agent blocks parse markdown only for their completed lines and
    /// render the unfinished tail line as plain text; parsed layouts are
    /// memoized per block so repeated layouts at the same width don't re-parse.
    ///
    /// The laid-out rows are pre-wrapped and memoized per `(width,
    /// generation)`, so scroll and selection queries (`line_count`,
    /// `visible_range`, `line_at_visual_row`, `selected_text`) operate on the
    /// cached visual rows instead of re-laying out the feed on every call.
    pub fn lines(&self, width: usize) -> Arc<FeedLines> {
        {
            let cache = self.layout_cache.borrow();
            if let Some(c) = cache.as_ref()
                && c.width == width
                && c.generation == self.generation
            {
                return c.lines.clone();
            }
        }
        let lines = Arc::new(self.compute_lines(width));
        #[cfg(test)]
        self.layout_computes.set(self.layout_computes.get() + 1);
        *self.layout_cache.borrow_mut() = Some(LayoutCache {
            width,
            generation: self.generation,
            lines: Arc::clone(&lines),
        });
        lines
    }

    /// Number of full layout passes so far (test-only).
    #[cfg(test)]
    pub(crate) fn layout_computes(&self) -> usize {
        self.layout_computes.get()
    }

    /// Total bytes parsed by markdown_to_styled during agent_block_lines (test-only).
    #[cfg(test)]
    pub(crate) fn markdown_bytes_parsed(&self) -> usize {
        self.markdown_bytes_parsed.get()
    }

    #[cfg(test)]
    pub(crate) fn block_renders(&self) -> usize {
        self.block_renders.get()
    }

    /// Lay out every block at `width`. Called by `lines` on a cache miss.
    fn compute_lines(&self, width: usize) -> FeedLines {
        let mut segments = Vec::new();
        if self.pruned_blocks > 0 {
            segments.push(Arc::new(plain_block_lines(
                &Block::new(
                    BlockStyle::System,
                    format!("[{} earlier feed blocks omitted]", self.pruned_blocks),
                ),
                width,
            )));
        }
        for block in &self.blocks {
            segments.extend(block_segments(self, block, width));
        }
        FeedLines::new(segments)
    }

    /// Total number of visible rows for the given width.
    #[cfg(test)]
    pub fn line_count(&self, width: usize) -> usize {
        self.lines(width).len()
    }

    /// Return the index of the first and last `LineEntry` that would be visible
    /// in a viewport of `viewport_height` rows with `scroll_offset`.
    ///
    /// `scroll_offset == 0` means "stick to the bottom" (auto-scroll).
    #[cfg(test)]
    pub fn visible_range(
        &self,
        width: usize,
        scroll_offset: usize,
        viewport_height: usize,
    ) -> (usize, usize) {
        let total = self.line_count(width);
        let visible = viewport_height.min(total);
        let auto_scroll = scroll_offset == 0;

        let start = if auto_scroll {
            total.saturating_sub(visible)
        } else {
            total.saturating_sub((scroll_offset + visible).min(total))
        };
        let end = (start + visible).min(total);
        (start, end)
    }

    /// Map a screen row (relative to the top of the viewport) to a `LineEntry`
    /// index in `lines(width)`.
    ///
    /// Returns `None` when the row is padding above bottom-aligned content or
    /// falls past the last visible line.
    #[cfg(test)]
    pub fn line_at_visual_row(
        &self,
        width: usize,
        scroll_offset: usize,
        viewport_height: usize,
        row: u16,
    ) -> Option<usize> {
        let total = self.line_count(width);
        if total == 0 {
            return None;
        }
        let visible = viewport_height.min(total);
        let auto_scroll = scroll_offset == 0;
        let pad = if auto_scroll && total < viewport_height {
            viewport_height - total
        } else {
            0
        };

        let row = row as usize;
        if row < pad {
            return None;
        }

        let start = if auto_scroll {
            total.saturating_sub(visible)
        } else {
            total.saturating_sub((scroll_offset + visible).min(total))
        };

        let lines = self.lines(width);
        let mut visual_row = pad;
        let mut idx = start;
        while idx < lines.len() {
            if visual_row == row {
                return Some(idx);
            }
            visual_row += 1;
            idx += 1;
        }
        None
    }

    /// Concatenate the text of all visible lines in the given range.
    #[cfg(test)]
    pub fn selected_text(&self, width: usize, start: usize, end: usize) -> Option<String> {
        let lines = self.lines(width);
        let (lo, hi) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let mut result = String::new();
        for i in lo..=hi {
            if let Some(entry) = lines.get(i) {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str(&entry.text);
            }
        }
        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }

    fn prune_completed_prefix(&mut self) {
        let mut removed = false;
        while self.blocks.len() > 1
            && (self.blocks.len() > MAX_FEED_BLOCKS || self.total_bytes > MAX_FEED_BYTES)
        {
            let running = self.blocks.iter().position(|block| block.running);
            let block = if let Some(running) = running {
                // Never shift a live block's externally tracked index. Shed
                // the oldest completed block appended after it; the completed
                // prefix is compacted as soon as the stream finalizes.
                self.blocks
                    .iter()
                    .enumerate()
                    .skip(running + 1)
                    .find_map(|(index, block)| (!block.running).then_some(index))
                    .and_then(|index| self.blocks.remove(index))
            } else {
                self.blocks.pop_front()
            };
            if let Some(block) = block {
                self.total_bytes = self.total_bytes.saturating_sub(block.text.len());
                self.pruned_blocks = self.pruned_blocks.saturating_add(1);
                removed = true;
            } else {
                break;
            }
        }
        if removed {
            self.generation = self.generation.wrapping_add(1);
            self.retention_generation = self.retention_generation.wrapping_add(1);
        }
    }

    #[cfg(test)]
    pub(crate) fn retention_limits_for_test() -> (usize, usize, usize) {
        (MAX_FEED_BLOCKS, MAX_FEED_BYTES, MAX_BLOCK_BYTES)
    }

    #[cfg(test)]
    pub(crate) fn total_bytes_for_test(&self) -> usize {
        self.total_bytes
    }
}

fn compact_oversized_block(text: &mut String) -> bool {
    if text.len() <= MAX_BLOCK_BYTES {
        return false;
    }
    let mut start = text.len().saturating_sub(RETAINED_BLOCK_BYTES);
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    let suffix = text[start..].to_string();
    text.clear();
    text.push_str(OMITTED_PREFIX);
    text.push_str(&suffix);
    true
}

fn block_segments(feed: &Feed, block: &Block, width: usize) -> Vec<Arc<Vec<LineEntry>>> {
    {
        let cache = block.render_cache.borrow();
        if let Some(cache) = cache.as_ref()
            && cache.width == width
            && cache.revision == block.revision
        {
            return cache.segments.clone();
        }
    }

    #[cfg(test)]
    feed.block_renders.set(feed.block_renders.get() + 1);
    let segments = match block.style {
        BlockStyle::Agent => agent_block_segments(feed, block, width),
        _ => vec![Arc::new(plain_block_lines(block, width))],
    };
    *block.render_cache.borrow_mut() = Some(BlockRenderCache {
        width,
        revision: block.revision,
        segments: segments.clone(),
    });
    segments
}

fn plain_block_lines(block: &Block, width: usize) -> Vec<LineEntry> {
    let mut result = Vec::new();
    let color = block.style.color();
    for line in block.text.split('\n') {
        let trimmed = line.trim_end_matches('\r');
        if trimmed.is_empty() {
            result.push(LineEntry {
                text: CompactString::new(""),
                color,
            });
        } else {
            for chunk in word_wrap(trimmed, width) {
                result.push(LineEntry { text: chunk, color });
            }
        }
    }
    result
}

/// Split a completed line into a fence delimiter run, if it is one.
///
/// Returns the delimiter character, its run length, the line's indentation and
/// the text after the run.
fn fence_delimiter(line: &str) -> Option<(u8, usize, usize, &str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let character = match rest.as_bytes().first() {
        Some(b'`') => b'`',
        Some(b'~') => b'~',
        _ => return None,
    };
    let length = rest.bytes().take_while(|byte| *byte == character).count();
    if length < 3 {
        return None;
    }
    Some((character, length, indent, &rest[length..]))
}

/// Whether `line` opens a fenced code block, per CommonMark: a backtick fence's
/// info string may not contain a backtick.
fn fence_opening(line: &str) -> Option<(u8, usize, usize)> {
    let (character, length, indent, info) = fence_delimiter(line)?;
    if character == b'`' && info.contains('`') {
        return None;
    }
    Some((character, length, indent))
}

/// Whether `line` closes `open`: the same character, at least as long as the
/// opening run, and nothing but whitespace after it. A shorter run, a different
/// character or trailing text stays ordinary code content.
fn fence_closing(line: &str, open: &OpenFence) -> bool {
    match fence_delimiter(line) {
        Some((character, length, _, suffix)) => {
            character == open.character && length >= open.length && suffix.trim().is_empty()
        }
        None => false,
    }
}

/// The last finalized markdown boundary in `text[start..up_to]`, plus the fence
/// open at that boundary.
///
/// A blank line is a boundary only at top level: not inside a fenced code
/// block, an indented code block, or a loose list. Inside a *replayable* fence
/// (one opened at top level) every completed line is a boundary, because code
/// content renders verbatim and the opening delimiter alone reproduces the
/// parser state — this is what keeps a long fenced response linear instead of
/// re-parsing the whole fence for every new line.
///
/// Constructs that publish no boundary of their own — a paragraph, a tight
/// list, a fence inside a list — would otherwise be re-parsed in full for every
/// completed line. Once such a run exceeds `MAX_UNSTABLE_WINDOW` bytes the
/// boundary is forced forward, which bounds the parsing a single streamed chunk
/// can cost. A forced split can render a very long unbroken construct slightly
/// differently while it streams; finalizing the block re-parses the whole text,
/// so the completed rendering is unaffected.
///
/// `start` must be a boundary previously returned by this function, and
/// `entry_fence` the fence reported with it.
fn find_stable_boundary(
    text: &str,
    start: usize,
    up_to: usize,
    entry_fence: Option<&OpenFence>,
) -> (usize, Option<OpenFence>) {
    let up_to = up_to.min(text.len());
    let start = start.min(up_to);
    let search_text = &text[start..up_to];

    let mut fence: Option<OpenFence> = entry_fence.cloned();
    let mut in_list = false;
    let mut in_indented_code = false;
    let mut deferred_blank = None;
    let mut last_stable_pos = start;
    let mut last_stable_fence = entry_fence.cloned();
    let mut byte_pos = start;

    // split_inclusive excludes a synthetic empty line after a trailing newline,
    // so every boundary we return is an actual byte offset in `text`.
    for completed_line in search_text.split_inclusive('\n') {
        if !completed_line.ends_with('\n') {
            break;
        }
        let line = completed_line.strip_suffix('\n').unwrap_or(completed_line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let line_end = byte_pos + completed_line.len();

        if let Some(open) = fence.clone() {
            if fence_closing(line, &open) {
                fence = None;
                if open.replayable {
                    last_stable_pos = line_end;
                    last_stable_fence = None;
                }
            } else if open.replayable {
                last_stable_pos = line_end;
                last_stable_fence = Some(open);
            }
            byte_pos = line_end;
            continue;
        }

        let trimmed = line.trim_start();
        let indent = line.len().saturating_sub(trimmed.len());
        if trimmed.is_empty() {
            if in_list || in_indented_code {
                // A loose list or indented code block may continue after one or
                // more blank lines. Wait for the next content line before
                // deciding whether this blank actually closed the construct.
                deferred_blank.get_or_insert(line_end);
            } else {
                last_stable_pos = line_end;
                last_stable_fence = None;
            }
            byte_pos = line_end;
            continue;
        }

        if let Some(blank_end) = deferred_blank.take() {
            let continues_list = in_list && (is_list_item(trimmed) || indent > 0);
            let continues_indented_code = in_indented_code && indent >= 4;
            if !continues_list && !continues_indented_code {
                last_stable_pos = blank_end;
                last_stable_fence = None;
                in_list = false;
                in_indented_code = false;
            }
        }

        if let Some((character, length, _)) = fence_opening(line) {
            let open = OpenFence {
                opener: CompactString::from(line),
                character,
                length,
                replayable: !in_list && !in_indented_code,
            };
            if open.replayable {
                last_stable_pos = line_end;
                last_stable_fence = Some(open.clone());
            }
            fence = Some(open);
            byte_pos = line_end;
            continue;
        }

        if is_list_item(trimmed) {
            in_list = true;
            in_indented_code = false;
        } else if indent >= 4 && !in_list {
            in_indented_code = true;
        }

        if line_end - last_stable_pos > MAX_UNSTABLE_WINDOW {
            last_stable_pos = line_end;
            last_stable_fence = None;
            in_list = false;
            in_indented_code = false;
            deferred_blank = None;
        }

        byte_pos = line_end;
    }

    (last_stable_pos, last_stable_fence)
}

/// Largest run of text without a natural markdown boundary that is re-parsed on
/// every streamed chunk before a boundary is forced.
const MAX_UNSTABLE_WINDOW: usize = 4 * 1024;

fn is_list_item(trimmed: &str) -> bool {
    if ["- ", "+ ", "* "]
        .iter()
        .any(|marker| trimmed.starts_with(marker))
    {
        return true;
    }

    let digit_count = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    digit_count > 0
        && trimmed
            .get(digit_count..)
            .is_some_and(|suffix| suffix.starts_with(". ") || suffix.starts_with(") "))
}

/// Link and footnote definitions can change the rendering of references that
/// appeared earlier in the document. When one arrives in an appended chunk,
/// the cached prefix is no longer independent and must be parsed again.
fn contains_global_markdown_definition(text: &str) -> bool {
    text.lines().any(|line| {
        let indent = line.bytes().take_while(|byte| *byte == b' ').count();
        if indent > 3 {
            return false;
        }
        let trimmed = &line[indent..];
        trimmed.starts_with('[') && trimmed.contains("]:")
    })
}

/// Lay out an agent block as shared completed-markdown and ephemeral tail
/// segments. Appending a token to the tail never clones completed rows.
fn agent_block_segments(feed: &Feed, block: &Block, width: usize) -> Vec<Arc<Vec<LineEntry>>> {
    let mut segments = agent_completed_lines(feed, block, width);
    let tail = agent_tail_lines(block, width, segments.is_empty());
    if !tail.is_empty() {
        segments.push(Arc::new(tail));
    }
    segments
}

/// Longest stable segment that a newly finalized region is merged into rather
/// than appended after, so a long stream does not accumulate one segment per
/// chunk.
const MD_SEGMENT_COALESCE_ROWS: usize = 64;

/// Render `text` knowing which fence, if any, is open at its start.
///
/// Replaying the opening delimiter reproduces the parser state, so the rows for
/// a suffix that begins inside a fence are exactly the rows a full parse would
/// place there. `pulldown-cmark` closes an unterminated fence at end of input,
/// which emits the code block's trailing empty row and the blank separator
/// after it. Both belong to the fence's real close, so they are dropped
/// whenever the region itself ends inside the fence and re-appear once a region
/// contains the close (or is the last region of a running block).
fn render_region(
    feed: &Feed,
    text: &str,
    entry_fence: Option<&OpenFence>,
    width: usize,
    ends_inside_fence: bool,
) -> Vec<LineEntry> {
    let mut rows = match entry_fence {
        Some(fence) => {
            let mut replayed = String::with_capacity(fence.opener.len() + 1 + text.len());
            replayed.push_str(&fence.opener);
            replayed.push('\n');
            replayed.push_str(text);
            parse_agent_markdown(feed, &replayed, width)
        }
        None => parse_agent_markdown(feed, text, width),
    };
    if ends_inside_fence {
        // Closing a code block emits the blank separator row, and the code
        // accumulator's trailing newline emits one empty code row. Both belong
        // to the fence's real close, which has not been seen yet.
        for _ in 0..2 {
            if rows.last().is_some_and(|row| row.text.is_empty()) {
                rows.pop();
            }
        }
    }
    rows
}

/// Lay out the completed portion of an agent block as markdown.
///
/// The parse is memoized in the block's `MdCache`, which tracks a stable
/// boundary where markdown is finalized. Extending past that boundary only
/// re-parses `stable_len..completed_len`, and finalized rows are kept as shared
/// segments so a streaming chunk never copies the completed prefix.
fn agent_completed_lines(feed: &Feed, block: &Block, width: usize) -> Vec<Arc<Vec<LineEntry>>> {
    // Text parsed as markdown: the whole block once finalized, or only the
    // completed lines (up to the last newline) while streaming.
    let completed_len = if block.running {
        match block.text.rfind('\n') {
            Some(idx) => idx + 1,
            None => 0,
        }
    } else {
        block.text.len()
    };

    // Try an exact cache hit before scanning markdown boundaries. Appending an
    // unfinished tail leaves completed_len unchanged and needs no markdown work.
    if let Some(cached) = cached_agent_lines(block, width, completed_len) {
        return cached;
    }

    // Mutators that can replace text clear md_cache, so a same-width cache with
    // a shorter parsed prefix is known to be an append-only extension.
    let incremental_base = {
        let cache = block.md_cache.borrow();
        cache.as_ref().and_then(|cache| {
            (cache.width == width && cache.parsed_len < completed_len).then(|| {
                (
                    cache.stable_len,
                    cache.stable_fence.clone(),
                    cache.stable_segments.clone(),
                    cache.has_rows(),
                    cache.parsed_len,
                )
            })
        })
    };

    if let Some((previous_stable_len, previous_fence, mut stable_segments, mut has_rows, _)) =
        incremental_base.filter(
            |(previous_stable_len, previous_fence, _, _, previous_parsed_len)| {
                // A link or footnote definition can change how an earlier reference
                // renders, so its arrival invalidates the cached prefix. Nothing
                // inside a fence is a definition, so a region that stays inside one
                // never needs the check.
                let (_, fence_at_end) = find_stable_boundary(
                    &block.text,
                    *previous_stable_len,
                    completed_len,
                    previous_fence.as_ref(),
                );
                let stays_in_fence = previous_fence.is_some() && fence_at_end == *previous_fence;
                stays_in_fence
                    || !contains_global_markdown_definition(
                        &block.text[*previous_parsed_len..completed_len],
                    )
            },
        )
    {
        let (stable_len, stable_fence) = find_stable_boundary(
            &block.text,
            previous_stable_len,
            completed_len,
            previous_fence.as_ref(),
        );

        if stable_len > previous_stable_len {
            let mut finalized = render_region(
                feed,
                &block.text[previous_stable_len..stable_len],
                previous_fence.as_ref(),
                width,
                stable_fence.is_some(),
            );
            if !has_rows && !finalized.is_empty() {
                prefix_agent_first_line(&mut finalized);
                has_rows = true;
            }
            push_stable_segment(&mut stable_segments, finalized);
        }

        let mut suffix = render_region(
            feed,
            &block.text[stable_len..completed_len],
            stable_fence.as_ref(),
            width,
            false,
        );
        if !has_rows && !suffix.is_empty() {
            prefix_agent_first_line(&mut suffix);
        }

        let cache = MdCache {
            width,
            stable_len,
            stable_fence,
            stable_segments,
            parsed_len: completed_len,
            suffix: Arc::new(suffix),
        };
        let segments = cache.segments();
        *block.md_cache.borrow_mut() = Some(cache);
        return segments;
    }

    // Full re-parse: a new stable boundary must be established, or the width
    // changed and every row has to be laid out again.
    let (stable_len, stable_fence) = find_stable_boundary(&block.text, 0, completed_len, None);
    let mut stable_segments: Vec<Arc<Vec<LineEntry>>> = Vec::new();
    let mut has_rows = false;
    if stable_len > 0 {
        let mut finalized = render_region(
            feed,
            &block.text[..stable_len],
            None,
            width,
            stable_fence.is_some(),
        );
        if !finalized.is_empty() {
            prefix_agent_first_line(&mut finalized);
            has_rows = true;
        }
        stable_segments.push(Arc::new(finalized));
    }
    let mut suffix = render_region(
        feed,
        &block.text[stable_len..completed_len],
        stable_fence.as_ref(),
        width,
        false,
    );
    if !has_rows && !suffix.is_empty() {
        prefix_agent_first_line(&mut suffix);
    }

    let cache = MdCache {
        width,
        stable_len,
        stable_fence,
        stable_segments,
        parsed_len: completed_len,
        suffix: Arc::new(suffix),
    };
    let segments = cache.segments();
    *block.md_cache.borrow_mut() = Some(cache);
    segments
}

/// Append newly finalized rows, merging into a short trailing segment so the
/// segment list stays proportional to content rather than to chunk count.
fn push_stable_segment(segments: &mut Vec<Arc<Vec<LineEntry>>>, rows: Vec<LineEntry>) {
    if rows.is_empty() {
        return;
    }
    if let Some(last) = segments.last_mut()
        && last.len() < MD_SEGMENT_COALESCE_ROWS
    {
        let mut merged = last.as_ref().clone();
        merged.extend(rows);
        *last = Arc::new(merged);
        return;
    }
    segments.push(Arc::new(rows));
}

fn prefix_agent_first_line(lines: &mut [LineEntry]) {
    if let Some(first) = lines.first_mut()
        && !first.text.starts_with("< ")
    {
        first.text = CompactString::from(format!("< {}", first.text));
    }
}

fn parse_agent_markdown(feed: &Feed, text: &str, width: usize) -> Vec<LineEntry> {
    #[cfg(test)]
    feed.markdown_bytes_parsed
        .set(feed.markdown_bytes_parsed.get() + text.len());
    #[cfg(not(test))]
    let _ = feed;
    markdown_to_styled(text, width)
}

fn agent_tail_lines(block: &Block, width: usize, needs_prefix: bool) -> Vec<LineEntry> {
    if !block.running {
        return Vec::new();
    }
    let completed_len = block.text.rfind('\n').map_or(0, |idx| idx + 1);
    let tail = block.text[completed_len..].trim_end_matches('\r');
    if tail.is_empty() {
        return Vec::new();
    }
    let color = BlockStyle::Agent.color();
    let mut lines = word_wrap(tail, width)
        .into_iter()
        .map(|text| LineEntry { text, color })
        .collect::<Vec<_>>();
    if needs_prefix {
        prefix_agent_first_line(&mut lines);
    }
    lines
}

/// Return the memoized markdown layout when it matches `(width, parsed_len)`.
fn cached_agent_lines(
    block: &Block,
    width: usize,
    parsed_len: usize,
) -> Option<Vec<Arc<Vec<LineEntry>>>> {
    let cache = block.md_cache.borrow();
    let cache = cache.as_ref()?;
    if cache.width == width && cache.parsed_len == parsed_len {
        Some(cache.segments())
    } else {
        None
    }
}
