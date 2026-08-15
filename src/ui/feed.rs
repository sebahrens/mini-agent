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
    /// Total bytes sent to markdown_to_styled during agent_block_lines calls;
    /// test-only proof that streaming achieves sub-quadratic parsing.
    #[cfg(test)]
    markdown_bytes_parsed: std::cell::Cell<usize>,
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
            #[cfg(test)]
            markdown_bytes_parsed: std::cell::Cell::new(0),
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

    /// Total bytes parsed by markdown_to_styled during agent_block_lines (test-only).
    #[cfg(test)]
    pub(crate) fn markdown_bytes_parsed(&self) -> usize {
        self.markdown_bytes_parsed.get()
    }

    /// Lay out every block at `width`. Called by `lines` on a cache miss.
    fn compute_lines(&self, width: usize) -> Vec<LineEntry> {
        let mut result = Vec::new();
        for block in &self.blocks {
            match block.style {
                BlockStyle::Agent => {
                    let mut styled = agent_block_lines(self, block, width);
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
/// a fenced code block, indented code block, or loose list. `start` must
/// already be a known top-level boundary, which lets streaming callers scan
/// only the newly unstable suffix instead of repeatedly scanning the prefix.
///
/// Correctness note: This approach safely handles:
/// - Fenced code blocks (``` / ~~~) containing blank lines: blank lines inside
///   fences are not boundaries
/// - Loose lists: blank lines between list items don't close the list
///
/// Limitation: Within a single long fenced block with no blank lines at
/// top-level, we degrade to O(n²) per-line re-parsing within that block.
/// This is acceptable as long-lived fences are less common than mixed content.
fn find_stable_boundary(text: &str, start: usize, up_to: usize) -> usize {
    let up_to = up_to.min(text.len());
    let start = start.min(up_to);
    let search_text = &text[start..up_to];

    let mut in_fence = false;
    let mut fence_char: Option<char> = None; // '`' or '~'
    let mut in_list = false;
    let mut in_indented_code = false;
    let mut deferred_blank = None;
    let mut last_stable_pos = start;
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

        // Check if this line opens/closes a fence. Fences must start at line
        // beginning (with optional leading whitespace, which we skip).
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let fence_char_candidate = if trimmed.starts_with("```") { '`' } else { '~' };

            if in_fence && fence_char == Some(fence_char_candidate) {
                // Closing the fence.
                in_fence = false;
                fence_char = None;
            } else if !in_fence {
                // Opening a new fence.
                in_fence = true;
                fence_char = Some(fence_char_candidate);
            }
            byte_pos = line_end;
            continue;
        }

        if in_fence {
            byte_pos = line_end;
            continue;
        }

        let indent = line.len().saturating_sub(trimmed.len());
        if trimmed.is_empty() {
            if in_list || in_indented_code {
                // A loose list or indented code block may continue after one or
                // more blank lines. Wait for the next content line before
                // deciding whether this blank actually closed the construct.
                deferred_blank.get_or_insert(line_end);
            } else {
                last_stable_pos = line_end;
            }
            byte_pos = line_end;
            continue;
        }

        if let Some(blank_end) = deferred_blank.take() {
            let continues_list = in_list && (is_list_item(trimmed) || indent > 0);
            let continues_indented_code = in_indented_code && indent >= 4;
            if !continues_list && !continues_indented_code {
                last_stable_pos = blank_end;
                in_list = false;
                in_indented_code = false;
            }
        }

        if is_list_item(trimmed) {
            in_list = true;
            in_indented_code = false;
        } else if indent >= 4 && !in_list {
            in_indented_code = true;
        }

        byte_pos = line_end;
    }

    last_stable_pos
}

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

/// Lay out an agent block: markdown for completed lines, plain text for the
/// unfinished tail line of a still-streaming block.
///
/// The markdown parse of the completed prefix is memoized in the block's
/// `MdCache`. To avoid O(n^2) re-parsing during streaming, the cache tracks
/// a "stable boundary" (e.g., after a blank line) where markdown is finalized.
/// When completed_len extends within the same stable boundary, only the new
/// portion (stable_len..completed_len) is re-parsed and appended. Mutators
/// that rewrite text (`replace_last`, `finalize_last`) clear the cache explicitly.
fn agent_block_lines(feed: &Feed, block: &Block, width: usize) -> Vec<LineEntry> {
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
        let mut lines = cached;
        append_agent_tail(&mut lines, block, completed_len, width);
        return lines;
    }

    // Mutators that can replace text clear md_cache, so a same-width cache
    // with a shorter parsed prefix is known to be an append-only extension.
    let incremental_base = {
        let cache = block.md_cache.borrow();
        cache.as_ref().and_then(|cache| {
            (cache.width == width && cache.parsed_len < completed_len).then(|| {
                (
                    cache.stable_len,
                    cache.stable_lines.clone(),
                    cache.parsed_len,
                )
            })
        })
    };

    if let Some((previous_stable_len, previous_stable_lines, _previous_parsed_len)) =
        incremental_base.filter(|(_, _, previous_parsed_len)| {
            !contains_global_markdown_definition(&block.text[*previous_parsed_len..completed_len])
        })
    {
        let stable_len = find_stable_boundary(&block.text, previous_stable_len, completed_len);
        let suffix =
            parse_agent_markdown(feed, &block.text[previous_stable_len..completed_len], width);
        let mut lines = previous_stable_lines.clone();
        lines.extend_from_slice(&suffix);

        let stable_lines = if stable_len == previous_stable_len {
            previous_stable_lines
        } else if stable_len == completed_len {
            lines.clone()
        } else {
            let mut stable_lines = previous_stable_lines;
            stable_lines.extend(parse_agent_markdown(
                feed,
                &block.text[previous_stable_len..stable_len],
                width,
            ));
            stable_lines
        };

        *block.md_cache.borrow_mut() = Some(MdCache {
            width,
            stable_len,
            stable_lines,
            parsed_len: completed_len,
            lines: lines.clone(),
        });

        append_agent_tail(&mut lines, block, completed_len, width);
        return lines;
    }

    // Full re-parse: need to establish a new stable boundary or handle width change.
    let stable_len = find_stable_boundary(&block.text, 0, completed_len);
    let full_parsed = parse_agent_markdown(feed, &block.text[..completed_len], width);

    // Compute stable lines: parse only up to the stable boundary.
    let stable_lines = if stable_len == completed_len {
        full_parsed.clone()
    } else if stable_len > 0 {
        parse_agent_markdown(feed, &block.text[..stable_len], width)
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
    append_agent_tail(&mut lines, block, completed_len, width);
    lines
}

fn parse_agent_markdown(feed: &Feed, text: &str, width: usize) -> Vec<LineEntry> {
    #[cfg(test)]
    feed.markdown_bytes_parsed
        .set(feed.markdown_bytes_parsed.get() + text.len());
    #[cfg(not(test))]
    let _ = feed;
    markdown_to_styled(text, width)
}

fn append_agent_tail(
    lines: &mut Vec<LineEntry>,
    block: &Block,
    completed_len: usize,
    width: usize,
) {
    if block.running && completed_len < block.text.len() {
        let tail = block.text[completed_len..].trim_end_matches('\r');
        if !tail.is_empty() {
            let color = BlockStyle::Agent.color();
            for chunk in word_wrap(tail, width) {
                lines.push(LineEntry { text: chunk, color });
            }
        }
    }
}

/// Return the memoized markdown layout when it matches `(width, stable_len, parsed_len)`.
fn cached_agent_lines(block: &Block, width: usize, parsed_len: usize) -> Option<Vec<LineEntry>> {
    let cache = block.md_cache.borrow();
    let cache = cache.as_ref()?;
    if cache.width == width && cache.parsed_len == parsed_len {
        Some(cache.lines.clone())
    } else {
        None
    }
}
