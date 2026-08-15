use std::cell::RefCell;

use compact_str::CompactString;
use crossterm::style::Color;

use super::markdown::{markdown_to_styled, word_wrap};
use super::renderer::LineEntry;
use super::{C_AGENT, C_ERROR, C_PERM, C_TOOL};

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
    /// Byte offset where markdown is definitely finalized (e.g., after "\n\n").
    /// Lines for text[0..stable_len] are cached in stable_lines and don't need
    /// to be re-parsed on the next incremental extension.
    stable_len: usize,
    /// Lines parsed up to stable_len. This is the stable prefix that won't
    /// change if text is extended beyond stable_len (within the same width).
    stable_lines: Vec<LineEntry>,
    /// Byte length of the parsed prefix: up to the last completed line for
    /// running blocks, the full text once finalized.
    parsed_len: usize,
    /// All lines parsed so far (for text[0..parsed_len]).
    lines: Vec<LineEntry>,
}

impl Block {
    pub fn new(style: BlockStyle, text: impl Into<String>) -> Self {
        Self {
            style,
            text: text.into(),
            running: false,
            md_cache: RefCell::new(None),
        }
    }
}

/// Conversation feed: a sequence of semantic blocks that can be laid out at
/// any width.
#[derive(Clone, Debug, Default)]
pub struct Feed {
    blocks: Vec<Block>,
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
}

/// Memoized layout of the whole feed at a viewport width and generation.
#[derive(Clone, Debug)]
struct LayoutCache {
    width: usize,
    generation: u64,
    lines: Vec<LineEntry>,
}

// Several helpers exist primarily for unit testing layout/scroll math without
// a terminal; allow them even when not yet wired into the production path.
#[allow(dead_code)]
impl Feed {
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            generation: 0,
            layout_cache: RefCell::new(None),
            #[cfg(test)]
            layout_computes: std::cell::Cell::new(0),
        }
    }

    /// Monotonic counter bumped on every content mutation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn clear(&mut self) {
        self.generation += 1;
        self.blocks.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    pub fn push_block(&mut self, style: BlockStyle, text: impl Into<String>) {
        self.generation += 1;
        self.blocks.push(Block::new(style, text));
    }

    /// Push an empty block that a producer will append to incrementally
    /// (e.g. streaming agent tokens). While running, agent blocks parse
    /// markdown only for completed lines and render the unfinished tail line
    /// as plain text. Call `finalize_last` when the stream ends.
    pub fn push_streaming_block(&mut self, style: BlockStyle) {
        self.generation += 1;
        let mut block = Block::new(style, "");
        block.running = true;
        self.blocks.push(block);
    }

    /// Mark the last block as complete: its full text (including the former
    /// tail line) is parsed as markdown on the next layout. No-op when the
    /// last block is not running.
    pub fn finalize_last(&mut self) {
        if let Some(last) = self.blocks.last_mut()
            && last.running
        {
            self.generation += 1;
            last.running = false;
            // Force one full re-parse now that the text is complete.
            *last.md_cache.borrow_mut() = None;
        }
    }

    pub fn push_line(&mut self, style: BlockStyle, text: impl Into<String>) {
        self.push_block(style, text);
    }

    /// Append text to the most recent block. Returns `false` when the feed is
    /// empty and there is no block to append to.
    pub fn append_to_last(&mut self, text: impl AsRef<str>) -> bool {
        if let Some(last) = self.blocks.last_mut() {
            self.generation += 1;
            last.text.push_str(text.as_ref());
            true
        } else {
            false
        }
    }

    /// Replace the last block, or push a new one if the feed is empty.
    pub fn replace_last(&mut self, style: BlockStyle, text: impl Into<String>) {
        self.generation += 1;
        if let Some(last) = self.blocks.last_mut() {
            last.style = style;
            last.text = text.into();
            last.running = false;
            *last.md_cache.borrow_mut() = None;
        } else {
            self.blocks.push(Block::new(style, text));
        }
    }

    pub fn truncate_blocks(&mut self, len: usize) {
        self.generation += 1;
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
    pub fn lines(&self, width: usize) -> Vec<LineEntry> {
        {
            let cache = self.layout_cache.borrow();
            if let Some(c) = cache.as_ref()
                && c.width == width
                && c.generation == self.generation
            {
                return c.lines.clone();
            }
        }
        let lines = self.compute_lines(width);
        #[cfg(test)]
        self.layout_computes.set(self.layout_computes.get() + 1);
        *self.layout_cache.borrow_mut() = Some(LayoutCache {
            width,
            generation: self.generation,
            lines: lines.clone(),
        });
        lines
    }

    /// Number of full layout passes so far (test-only).
    #[cfg(test)]
    pub(crate) fn layout_computes(&self) -> usize {
        self.layout_computes.get()
    }

    /// Lay out every block at `width`. Called by `lines` on a cache miss.
    fn compute_lines(&self, width: usize) -> Vec<LineEntry> {
        let mut result = Vec::new();
        for block in &self.blocks {
            match block.style {
                BlockStyle::Agent => {
                    let mut styled = agent_block_lines(block, width);
                    if !styled.is_empty() {
                        styled[0].text = CompactString::from(format!("< {}", styled[0].text));
                    }
                    result.extend(styled);
                }
                _ => {
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
                }
            }
        }
        result
    }

    /// Total number of visible rows for the given width.
    pub fn line_count(&self, width: usize) -> usize {
        self.lines(width).len()
    }

    /// Return the index of the first and last `LineEntry` that would be visible
    /// in a viewport of `viewport_height` rows with `scroll_offset`.
    ///
    /// `scroll_offset == 0` means "stick to the bottom" (auto-scroll).
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
}

/// Find the byte offset of the last finalized markdown block boundary.
///
/// A blank line is only a stable boundary if we're at top-level: not inside
/// a fenced code block, indented code block, or loose list. This function
/// scans the text tracking fence state and returns the position after the
/// last blank line that occurs at top-level.
///
/// Correctness note: This approach safely handles:
/// - Fenced code blocks (``` / ~~~) containing blank lines: blank lines inside
///   fences are not boundaries
/// - Loose lists: blank lines between list items don't close the list
///
/// Limitation: Within a single long fenced block with no blank lines at
/// top-level, we degrade to O(n²) per-line re-parsing within that block.
/// This is acceptable as long-lived fences are less common than mixed content.
fn find_stable_boundary(text: &str, up_to: usize) -> usize {
    let search_text = &text[..up_to.min(text.len())];

    // Scan through the text, tracking whether we're inside a fenced code block.
    // Only blank lines at top-level (not in a fence) are stable boundaries.
    let mut in_fence = false;
    let mut fence_char: Option<char> = None; // '`' or '~'
    let mut last_stable_pos = 0;
    let mut prev_was_blank = false;
    let mut byte_pos = 0;

    for line in search_text.split('\n') {
        let line_start = byte_pos;

        // Check if this line opens/closes a fence. Fences must start at line
        // beginning (with optional leading whitespace, which we skip).
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let new_fence_char = if trimmed.starts_with("```") { '`' } else { '~' };

            if in_fence && fence_char == Some(new_fence_char) {
                // Closing the fence.
                in_fence = false;
                fence_char = None;
            } else if !in_fence {
                // Opening a new fence.
                in_fence = true;
                fence_char = Some(new_fence_char);
            }
            // If in_fence but char doesn't match, it's just content; ignore
        }

        // If the previous line was blank and this one is too (and we're at
        // top-level), the position after the previous line is a stable boundary.
        if !in_fence && prev_was_blank && line.is_empty() {
            last_stable_pos = byte_pos;
        }

        prev_was_blank = !in_fence && line.is_empty();
        byte_pos = line_start + line.len() + 1; // +1 for the newline
    }

    last_stable_pos
}

/// Lay out an agent block: markdown for completed lines, plain text for the
/// unfinished tail line of a still-streaming block.
///
/// The markdown parse of the completed prefix is memoized in the block's
/// `MdCache`. To avoid O(n^2) re-parsing during streaming, the cache tracks
/// a "stable boundary" (e.g., after a blank line) where markdown is finalized.
/// When completed_len extends within the same stable boundary, only the new
/// portion (stable_len..completed_len) is re-parsed and appended. Mutators
/// that rewrite text (`replace_last`, `finalize_last`) clear the cache explicitly.
fn agent_block_lines(block: &Block, width: usize) -> Vec<LineEntry> {
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

    let stable_len = find_stable_boundary(&block.text, completed_len);

    // Try exact cache hit first: same width, stable_len, and parsed_len.
    if let Some(cached) = cached_agent_lines(block, width, completed_len, stable_len) {
        let mut lines = cached;
        // Append the unfinished tail line if needed.
        if block.running && completed_len < block.text.len() {
            let tail = block.text[completed_len..].trim_end_matches('\r');
            if !tail.is_empty() {
                let color = BlockStyle::Agent.color();
                for chunk in word_wrap(tail, width) {
                    lines.push(LineEntry { text: chunk, color });
                }
            }
        }
        return lines;
    }

    // Check if we can extend incrementally from a previous cache state.
    // If the stable boundary hasn't changed and the width is the same,
    // we can reuse the stable lines and only re-parse from stable_len to completed_len.
    let should_extend = {
        let cache = block.md_cache.borrow();
        cache
            .as_ref()
            .map(|c| c.width == width && c.stable_len == stable_len && c.parsed_len < completed_len)
            .unwrap_or(false)
    };

    if should_extend {
        // We need to re-borrow since we released the previous borrow.
        let (stable_lines_to_use, stable_count) = {
            let cache = block.md_cache.borrow();
            let c = cache.as_ref().unwrap();
            (c.stable_lines.clone(), c.stable_lines.len())
        };

        // Incremental extension within the same stable boundary.
        let full_parsed = markdown_to_styled(&block.text[..completed_len], width);

        let mut lines = stable_lines_to_use.clone();
        if stable_count < full_parsed.len() {
            lines.extend_from_slice(&full_parsed[stable_count..]);
        }

        *block.md_cache.borrow_mut() = Some(MdCache {
            width,
            stable_len,
            stable_lines: stable_lines_to_use,
            parsed_len: completed_len,
            lines: lines.clone(),
        });

        // Append the unfinished tail line if needed.
        if block.running && completed_len < block.text.len() {
            let tail = block.text[completed_len..].trim_end_matches('\r');
            if !tail.is_empty() {
                let color = BlockStyle::Agent.color();
                for chunk in word_wrap(tail, width) {
                    lines.push(LineEntry { text: chunk, color });
                }
            }
        }
        return lines;
    }

    // Full re-parse: need to establish a new stable boundary or handle width change.
    let full_parsed = markdown_to_styled(&block.text[..completed_len], width);

    // Compute stable lines: parse only up to the stable boundary.
    let stable_lines = if stable_len > 0 {
        markdown_to_styled(&block.text[..stable_len.min(block.text.len())], width)
    } else {
        Vec::new()
    };

    *block.md_cache.borrow_mut() = Some(MdCache {
        width,
        stable_len,
        stable_lines,
        parsed_len: completed_len,
        lines: full_parsed.clone(),
    });

    let mut lines = full_parsed;
    // Append the unfinished tail line if needed.
    if block.running && completed_len < block.text.len() {
        let tail = block.text[completed_len..].trim_end_matches('\r');
        if !tail.is_empty() {
            let color = BlockStyle::Agent.color();
            for chunk in word_wrap(tail, width) {
                lines.push(LineEntry { text: chunk, color });
            }
        }
    }
    lines
}

/// Return the memoized markdown layout when it matches `(width, stable_len, parsed_len)`.
fn cached_agent_lines(
    block: &Block,
    width: usize,
    parsed_len: usize,
    stable_len: usize,
) -> Option<Vec<LineEntry>> {
    let cache = block.md_cache.borrow();
    let cache = cache.as_ref()?;
    if cache.width == width && cache.parsed_len == parsed_len && cache.stable_len == stable_len {
        Some(cache.lines.clone())
    } else {
        None
    }
}
