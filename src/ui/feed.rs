use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ops::Index;
use std::sync::Arc;
use std::time::Instant;

use crate::config::ReasoningDisplay;
use compact_str::CompactString;
use crossterm::style::Color;

use super::events::{Sanitized, sanitize_output};
use super::markdown::{StyledMarkdown, markdown_with_code_state, word_wrap};
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
    DiffAdd,
    DiffDel,
    DiffHunk,
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
            BlockStyle::DiffAdd => Color::Green,
            BlockStyle::DiffDel => Color::Red,
            BlockStyle::DiffHunk => Color::Cyan,
            BlockStyle::Error => C_ERROR,
            BlockStyle::System => Color::DarkGrey,
            BlockStyle::Welcome => Color::Cyan,
            BlockStyle::Permission => C_PERM,
            BlockStyle::Plain => Color::White,
        }
    }
}

/// Resolved foreground roles. Kept separate from content so switching themes
/// cannot change a block's identity, text, or markdown structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ForegroundPalette {
    pub user: Color,
    pub agent: Color,
    pub tool: Color,
    pub result: Color,
    pub code: Color,
    pub reasoning: Color,
    pub error: Color,
    pub warning: Color,
    pub diff_add: Color,
    pub diff_del: Color,
    pub status: Color,
    pub system: Color,
    pub permission: Color,
    pub link: Color,
    pub text: Color,
}

impl Default for ForegroundPalette {
    fn default() -> Self {
        Self {
            user: BlockStyle::User.color(),
            agent: BlockStyle::Agent.color(),
            tool: BlockStyle::Tool.color(),
            result: BlockStyle::ToolResult.color(),
            code: Color::DarkYellow,
            reasoning: BlockStyle::Reasoning.color(),
            error: BlockStyle::Error.color(),
            warning: Color::Yellow,
            diff_add: BlockStyle::DiffAdd.color(),
            diff_del: BlockStyle::DiffDel.color(),
            status: Color::DarkGrey,
            system: BlockStyle::System.color(),
            permission: BlockStyle::Permission.color(),
            link: Color::Cyan,
            text: Color::White,
        }
    }
}

impl ForegroundPalette {
    pub(crate) fn block_color(self, style: BlockStyle) -> Color {
        match style {
            BlockStyle::User => self.user,
            BlockStyle::Agent => self.agent,
            BlockStyle::Reasoning => self.reasoning,
            BlockStyle::Tool => self.tool,
            BlockStyle::ToolResult => self.result,
            BlockStyle::DiffAdd => self.diff_add,
            BlockStyle::DiffDel => self.diff_del,
            BlockStyle::DiffHunk | BlockStyle::Welcome => self.link,
            BlockStyle::Error => self.error,
            BlockStyle::System => self.system,
            BlockStyle::Permission => self.permission,
            BlockStyle::Plain => self.text,
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
    /// Markdown, syntax and highlight-budget state, allocated on the first
    /// agent layout so every other block keeps only a pointer-sized slot.
    agent: std::cell::OnceCell<Box<AgentRender>>,
    revision: u64,
    render_cache: RefCell<Option<BlockRenderCache>>,
    /// Wrapped rows of a plain-text body's completed lines (reasoning bodies
    /// and running non-agent blocks), so a streamed chunk wraps only the lines
    /// it completed plus the unfinished tail.
    plain_cache: RefCell<Option<PlainCache>>,
    /// Producer key (a tool-call id) that later blocks can be placed after,
    /// so a tool result lands under its own call rather than at the end.
    anchor: Option<CompactString>,
    /// The anchor this block was inserted after, keeping several blocks
    /// inserted for one anchor in insertion order.
    attached_to: Option<CompactString>,
    reasoning: Option<ReasoningBlock>,
}

/// Per-block state only agent (markdown) layouts use.
#[derive(Clone, Debug, Default)]
struct AgentRender {
    /// Memoized markdown layout. Interior mutability keeps `Feed::lines` a
    /// `&self` read; `Feed` mutators that rewrite block text invalidate it.
    md: RefCell<Option<MdCache>>,
    syntax: RefCell<super::highlight::TokenCache>,
    /// Incremental highlight-budget verdict for `text`, computed on first use
    /// and extended by appends so a repaint never rescans the whole block.
    /// `None` until a layout needs it, and again after the text is rewritten.
    budget: Cell<Option<super::highlight::BudgetScan>>,
}

#[derive(Clone, Debug)]
struct ReasoningBlock {
    chars: usize,
    started: Option<Instant>,
    elapsed_secs: Option<u64>,
    expanded: Option<bool>,
}

/// Wrapped rows for `text[..completed_len]` (whole lines) at `width`.
#[derive(Clone, Debug)]
struct PlainCache {
    width: usize,
    completed_len: usize,
    segments: Vec<Arc<Vec<LineEntry>>>,
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
    highlighting: bool,
    /// Lexical state at stable_len, paired with the exact replayable fence.
    stable_code: Option<super::highlight::CodeState>,
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
    /// Whether a stable region ended at a boundary forced by
    /// `MAX_UNSTABLE_WINDOW`. Such a split can render differently from a
    /// whole-text parse, so finalizing the block must re-parse from scratch.
    forced_split: bool,
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
    /// Whether the block's text fits the syntax-highlighting budget. Scans the
    /// text once; later appends extend the verdict incrementally.
    fn within_highlight_budget(&self) -> bool {
        let agent = self.agent_render();
        match agent.budget.get() {
            Some(scan) => scan.within(),
            None => {
                let scan = super::highlight::BudgetScan::of(&self.text);
                agent.budget.set(Some(scan));
                scan.within()
            }
        }
    }

    /// Record `appended` (already pushed onto `text`) in the budget verdict.
    fn note_appended(&self, appended: &str) {
        if let Some(agent) = self.agent.get()
            && let Some(mut scan) = agent.budget.get()
        {
            scan.push(appended);
            agent.budget.set(Some(scan));
        }
    }

    /// The agent layout state, allocated on first use.
    fn agent_render(&self) -> &AgentRender {
        self.agent.get_or_init(Default::default)
    }

    /// Drop the memoized markdown layout (a no-op before any agent layout).
    fn clear_md_cache(&self) {
        if let Some(agent) = self.agent.get() {
            *agent.md.borrow_mut() = None;
        }
    }

    /// Drop the memoized syntax tokens (a no-op before any agent layout).
    fn clear_syntax_cache(&self) {
        if let Some(agent) = self.agent.get() {
            agent.syntax.borrow_mut().clear();
        }
    }

    /// Forget the highlight-budget verdict (a no-op before any agent layout).
    fn clear_highlight_budget(&self) {
        if let Some(agent) = self.agent.get() {
            agent.budget.set(None);
        }
    }

    pub fn new(style: BlockStyle, text: impl Into<String>) -> Self {
        let mut text = terminal_safe(text.into());
        let reasoning = (style == BlockStyle::Reasoning).then(|| ReasoningBlock {
            chars: text.chars().count(),
            started: None,
            elapsed_secs: None,
            expanded: None,
        });
        compact_oversized_block(&mut text);
        Self {
            style,
            text,
            running: false,
            agent: std::cell::OnceCell::new(),
            revision: 0,
            render_cache: RefCell::new(None),
            plain_cache: RefCell::new(None),
            anchor: None,
            attached_to: None,
            reasoning,
        }
    }
}

/// `attached_to` key of the live progress block under an anchor. The control
/// character cannot occur in an anchor id, so it never matches a result.
fn progress_attachment(anchor: &str) -> CompactString {
    let mut key = CompactString::from("\u{1}progress:");
    key.push_str(anchor);
    key
}

/// Immutable, segmented visual rows. Indexing stays O(log blocks), while a
/// feed mutation shares every unchanged block's row allocation.
#[derive(Clone, Debug, Default)]
pub struct FeedLines {
    segments: Vec<Arc<Vec<LineEntry>>>,
    starts: Vec<usize>,
    len: usize,
    /// Set on a tail-first layout that does not start at the top of the feed.
    partial: bool,
}

impl FeedLines {
    pub fn len(&self) -> usize {
        self.len
    }

    fn push(&mut self, segment: Arc<Vec<LineEntry>>) {
        self.starts.push(self.len);
        self.len = self.len.saturating_add(segment.len());
        self.segments.push(segment);
    }

    /// Keep the first `segments` segments.
    fn truncate(&mut self, segments: usize) {
        if segments >= self.segments.len() {
            return;
        }
        self.len = self.starts[segments];
        self.segments.truncate(segments);
        self.starts.truncate(segments);
    }

    /// A copy of the first `segments` segments, sharing their rows.
    fn prefix(&self, segments: usize) -> Self {
        let segments = segments.min(self.segments.len());
        Self {
            segments: self.segments[..segments].to_vec(),
            starts: self.starts[..segments].to_vec(),
            len: self.starts.get(segments).copied().unwrap_or(self.len),
            partial: self.partial,
        }
    }

    pub fn get(&self, index: usize) -> Option<&LineEntry> {
        if index >= self.len {
            return None;
        }
        let segment = self.starts.partition_point(|start| *start <= index) - 1;
        self.segments[segment].get(index - self.starts[segment])
    }

    /// Whether these rows start at the top of the feed. Only a tail-first
    /// layout ([`Feed::lines_tail`]) can be incomplete.
    pub(crate) fn is_complete(&self) -> bool {
        !self.partial
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
    palette: ForegroundPalette,
    monochrome: bool,
    blocks: VecDeque<Block>,
    total_bytes: usize,
    pruned_blocks: u64,
    retention_generation: u64,
    /// Bumped by every content mutation. The renderer compares generations to
    /// know whether the chat viewport needs a redraw, which also catches
    /// mutations made through `Renderer::feed_mut()`.
    generation: u64,
    reasoning_display: ReasoningDisplay,
    /// Pre-wrapped visual rows for the last requested width. Scroll and
    /// selection queries reuse these rows instead of re-laying out the whole
    /// feed each time; invalidated by any content mutation (generation bump)
    /// or a width change.
    layout_cache: RefCell<Option<LayoutCache>>,
    /// Lowest block index mutated since `layout_cache` was built
    /// (`usize::MAX` when none). Rows of the blocks before it are reused.
    layout_dirty_from: Cell<usize>,
    /// Index of the running reasoning block (there is at most one), kept
    /// exact at every index-shifting site so ticks, deltas and finishing
    /// touch one block instead of scanning the feed.
    running_reasoning: Option<usize>,
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
    /// Bytes word-wrapped for incrementally laid-out plain bodies (test-only
    /// proof that streaming reasoning re-wraps only new text).
    #[cfg(test)]
    wrap_bytes: std::cell::Cell<usize>,
    /// Blocks visited while rebuilding the feed layout (test-only proof that
    /// a token appended to the last block reuses every earlier block's rows).
    #[cfg(test)]
    blocks_laid_out: std::cell::Cell<usize>,
}

/// Memoized layout of the whole feed at a viewport width and generation.
#[derive(Clone, Debug)]
struct LayoutCache {
    width: usize,
    generation: u64,
    lines: Arc<FeedLines>,
    /// Segments contributed by the omitted-blocks notice (0 while the
    /// layout is partial).
    notice_segments: usize,
    /// First block laid out. A tail-first layout ([`Feed::lines_tail`])
    /// covers only `blocks[first_block..]`; 0 means the layout is complete.
    first_block: usize,
    /// Segments contributed by each laid-out block, in order.
    block_segments: Vec<usize>,
}

impl LayoutCache {
    fn complete(&self) -> bool {
        self.first_block == 0
    }
}

fn mark_dirty(dirty_from: &Cell<usize>, index: usize) {
    dirty_from.set(dirty_from.get().min(index));
}

impl Feed {
    pub fn new() -> Self {
        Self {
            palette: ForegroundPalette::default(),
            monochrome: false,
            blocks: VecDeque::new(),
            total_bytes: 0,
            pruned_blocks: 0,
            retention_generation: 0,
            generation: 0,
            reasoning_display: ReasoningDisplay::Collapsed,
            layout_cache: RefCell::new(None),
            layout_dirty_from: Cell::new(usize::MAX),
            running_reasoning: None,
            #[cfg(test)]
            layout_computes: std::cell::Cell::new(0),
            #[cfg(test)]
            markdown_bytes_parsed: std::cell::Cell::new(0),
            #[cfg(test)]
            block_renders: std::cell::Cell::new(0),
            #[cfg(test)]
            wrap_bytes: std::cell::Cell::new(0),
            #[cfg(test)]
            blocks_laid_out: std::cell::Cell::new(0),
        }
    }

    /// Record a content mutation of the block at `index` (and possibly every
    /// block after it): bump the generation and mark the layout dirty from
    /// there.
    fn touch(&mut self, index: usize) {
        self.generation = self.generation.wrapping_add(1);
        mark_dirty(&self.layout_dirty_from, index);
    }

    pub(crate) fn set_palette(&mut self, palette: ForegroundPalette) {
        if self.palette == palette {
            return;
        }
        self.palette = palette;
        self.touch(0);
        *self.layout_cache.borrow_mut() = None;
        for block in &self.blocks {
            *block.render_cache.borrow_mut() = None;
            block.clear_md_cache();
            *block.plain_cache.borrow_mut() = None;
            block.clear_syntax_cache();
        }
    }

    pub(crate) fn set_monochrome(&mut self, monochrome: bool) {
        if self.monochrome == monochrome {
            return;
        }
        self.monochrome = monochrome;
        self.touch(0);
        *self.layout_cache.borrow_mut() = None;
        for block in &self.blocks {
            *block.render_cache.borrow_mut() = None;
            block.clear_md_cache();
            *block.plain_cache.borrow_mut() = None;
            block.clear_syntax_cache();
        }
    }

    /// Monotonic counter bumped on every content mutation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn reasoning_display(&self) -> ReasoningDisplay {
        self.reasoning_display
    }

    pub(crate) fn set_reasoning_display(&mut self, display: ReasoningDisplay) {
        if self.reasoning_display == display {
            return;
        }
        self.reasoning_display = display;
        self.touch(0);
        for block in &mut self.blocks {
            if let Some(reasoning) = block.reasoning.as_mut() {
                reasoning.expanded = None;
                *block.render_cache.borrow_mut() = None;
            }
        }
    }

    /// Toggle only the reasoning header occupying this visual row. Body rows
    /// remain selectable, including URLs, instead of acting as disclosure controls.
    pub(crate) fn toggle_reasoning_at_row(&mut self, width: usize, row: usize) -> bool {
        if self.reasoning_display == ReasoningDisplay::Off {
            return false;
        }
        let mut start = if self.pruned_blocks > 0 {
            plain_block_lines(
                &Block::new(
                    BlockStyle::System,
                    format!("[{} earlier feed blocks omitted]", self.pruned_blocks),
                ),
                width,
                self.palette,
            )
            .len()
        } else {
            0
        };
        let position = self.blocks.iter().enumerate().find_map(|(index, block)| {
            let len = block_segments(self, block, width)
                .iter()
                .map(|segment| segment.len())
                .sum::<usize>();
            let hit = block.reasoning.is_some() && row == start && len > 0;
            start = start.saturating_add(len);
            hit.then_some(index)
        });
        let Some(index) = position else {
            return false;
        };
        let block = &mut self.blocks[index];
        let reasoning = block.reasoning.as_mut().expect("reasoning header");
        let expanded = reasoning
            .expanded
            .unwrap_or(self.reasoning_display == ReasoningDisplay::Full);
        reasoning.expanded = Some(!expanded);
        *block.render_cache.borrow_mut() = None;
        self.touch(index);
        true
    }

    /// Update live headers at most once per elapsed second. Completed durations
    /// are frozen, and unchanged blocks retain their layout allocations.
    pub(crate) fn tick_reasoning_at(&mut self, now: Instant) {
        let Some(index) = self.live_reasoning() else {
            return;
        };
        let block = &mut self.blocks[index];
        let Some(reasoning) = block.reasoning.as_mut() else {
            return;
        };
        let Some(started) = reasoning.started else {
            return;
        };
        let elapsed = now.saturating_duration_since(started).as_secs();
        if reasoning.elapsed_secs != Some(elapsed) {
            reasoning.elapsed_secs = Some(elapsed);
            *block.render_cache.borrow_mut() = None;
            self.generation = self.generation.wrapping_add(1);
            mark_dirty(&self.layout_dirty_from, index);
        }
    }

    /// Index of the running reasoning block, from the tracked slot. Tests
    /// check it against a full scan on every use.
    fn live_reasoning(&self) -> Option<usize> {
        #[cfg(test)]
        assert_eq!(
            self.running_reasoning,
            self.blocks
                .iter()
                .rposition(|block| block.running && block.reasoning.is_some()),
            "running_reasoning drifted"
        );
        self.running_reasoning
    }

    /// A block was inserted at `position`: later indices move up.
    fn note_inserted_at(&mut self, position: usize) {
        if let Some(index) = self.running_reasoning.as_mut()
            && *index >= position
        {
            *index += 1;
        }
    }

    /// The block at `position` was removed: later indices move down.
    fn note_removed_at(&mut self, position: usize) {
        if let Some(index) = self.running_reasoning {
            self.running_reasoning = match index.cmp(&position) {
                std::cmp::Ordering::Less => Some(index),
                std::cmp::Ordering::Equal => None,
                std::cmp::Ordering::Greater => Some(index - 1),
            };
        }
    }

    #[cfg(test)]
    pub(crate) fn append_reasoning(&mut self, text: &str) {
        let index = self.reasoning_index();
        self.append_to(index, text);
    }

    /// Append an already sanitised reasoning delta to the live reasoning block.
    pub(crate) fn append_reasoning_sanitized(&mut self, text: &Sanitized) {
        let index = self.reasoning_index();
        self.append_sanitized(index, text);
    }

    /// Index of the running reasoning block, pushing one when none is live.
    fn reasoning_index(&mut self) -> usize {
        match self.live_reasoning() {
            Some(index) => index,
            None => {
                self.push_streaming_block(BlockStyle::Reasoning);
                self.blocks.len() - 1
            }
        }
    }

    pub(crate) fn finish_reasoning(&mut self) {
        if let Some(index) = self.live_reasoning() {
            self.finalize_block(index);
        }
    }

    pub fn clear(&mut self) {
        self.touch(0);
        self.blocks.clear();
        self.running_reasoning = None;
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
        self.touch(self.blocks.len());
        let block = Block::new(style, text);
        self.total_bytes = self.total_bytes.saturating_add(block.text.len());
        self.blocks.push_back(block);
        self.prune_completed_prefix();
    }

    /// Push a completed block that [`Feed::place_after_anchor`] can later
    /// place blocks after. An empty anchor is no anchor.
    pub fn push_anchored_block(
        &mut self,
        anchor: &str,
        style: BlockStyle,
        text: impl Into<String>,
    ) {
        let mut block = Block::new(style, text);
        block.anchor = (!anchor.is_empty()).then(|| CompactString::from(anchor));
        self.touch(self.blocks.len());
        self.total_bytes = self.total_bytes.saturating_add(block.text.len());
        self.blocks.push_back(block);
        self.prune_completed_prefix();
    }

    /// Insert a completed block directly after the most recent block
    /// anchored at `anchor`, after any blocks already inserted there, so
    /// results read in call order under their calls. When no such block is
    /// retained, or inserting would shift a still-running block whose index
    /// a producer tracks, the block is appended instead. Returns whether it
    /// was placed under the anchor.
    pub fn place_after_anchor(
        &mut self,
        anchor: &str,
        style: BlockStyle,
        text: impl Into<String>,
    ) -> bool {
        let Some(position) = self.anchor_insert_position(anchor) else {
            self.push_block(style, text);
            return false;
        };
        let mut block = Block::new(style, text);
        block.attached_to = Some(CompactString::from(anchor));
        self.touch(position);
        self.total_bytes = self.total_bytes.saturating_add(block.text.len());
        self.blocks.insert(position, block);
        self.note_inserted_at(position);
        self.prune_completed_prefix();
        true
    }

    /// Place a run of single-line rows under `anchor` like repeated
    /// [`Feed::place_after_anchor`] calls, but fold consecutive rows of one
    /// style into a single multi-line block (a diff hunk header always starts
    /// its own block). The feed wraps and colours every line of a block on its
    /// own, so the rendered rows are those of one block per row, without a
    /// block, caches and retention slot per diff line.
    pub fn place_rows_after_anchor<S: AsRef<str>>(
        &mut self,
        anchor: &str,
        rows: impl IntoIterator<Item = (BlockStyle, S)>,
    ) {
        let mut run: Option<(BlockStyle, String)> = None;
        let mut runs: Vec<(BlockStyle, String)> = Vec::new();
        for (style, text) in rows {
            match run.as_mut() {
                Some((run_style, run_text))
                    if *run_style == style && style != BlockStyle::DiffHunk =>
                {
                    run_text.push('\n');
                    run_text.push_str(text.as_ref());
                }
                _ => {
                    runs.extend(run.take());
                    run = Some((style, text.as_ref().to_owned()));
                }
            }
        }
        runs.extend(run);
        let Some(mut position) = self.anchor_insert_position(anchor) else {
            for (style, text) in runs {
                self.push_block(style, text);
            }
            return;
        };
        self.touch(position);
        for (style, text) in runs {
            let mut block = Block::new(style, text);
            block.attached_to = Some(CompactString::from(anchor));
            self.total_bytes = self.total_bytes.saturating_add(block.text.len());
            self.blocks.insert(position, block);
            self.note_inserted_at(position);
            position += 1;
        }
        self.prune_completed_prefix();
    }

    /// Show `text` as the live progress block under `anchor` (a running tool
    /// call's output preview), replacing the previous preview in place. The
    /// first preview is placed like [`Feed::place_after_anchor`]; nothing is
    /// shown when the anchor is no longer retained. Results placed under the
    /// anchor later go above the preview, which
    /// [`Feed::clear_anchor_progress`] removes.
    pub fn set_anchor_progress(
        &mut self,
        anchor: &str,
        style: BlockStyle,
        text: impl Into<String>,
    ) -> bool {
        if anchor.is_empty() {
            return false;
        }
        let key = progress_attachment(anchor);
        if let Some(block) = self
            .blocks
            .iter_mut()
            .rev()
            .find(|block| block.anchor.is_none() && block.attached_to.as_deref() == Some(&*key))
        {
            self.generation += 1;
            let old_len = block.text.len();
            block.style = style;
            block.text = terminal_safe(text.into());
            compact_oversized_block(&mut block.text);
            self.total_bytes = self
                .total_bytes
                .saturating_sub(old_len)
                .saturating_add(block.text.len());
            block.revision = block.revision.wrapping_add(1);
            block.clear_md_cache();
            block.clear_syntax_cache();
            *block.render_cache.borrow_mut() = None;
            return true;
        }
        let Some(position) = self.anchor_insert_position(anchor) else {
            return false;
        };
        let mut block = Block::new(style, text);
        block.attached_to = Some(key);
        self.generation += 1;
        self.total_bytes = self.total_bytes.saturating_add(block.text.len());
        self.blocks.insert(position, block);
        self.note_inserted_at(position);
        self.prune_completed_prefix();
        true
    }

    /// Remove the live progress block under `anchor`. It is kept (stale but
    /// harmless) when removing it would shift a still-running block whose
    /// index a producer tracks. Returns whether a block was removed.
    pub fn clear_anchor_progress(&mut self, anchor: &str) -> bool {
        if anchor.is_empty() {
            return false;
        }
        let key = progress_attachment(anchor);
        let Some(index) = self.blocks.iter().rposition(|block| {
            block.anchor.is_none() && block.attached_to.as_deref() == Some(&*key)
        }) else {
            return false;
        };
        if self
            .blocks
            .iter()
            .skip(index + 1)
            .any(|block| block.running)
        {
            return false;
        }
        if let Some(block) = self.blocks.remove(index) {
            self.note_removed_at(index);
            self.generation += 1;
            self.total_bytes = self.total_bytes.saturating_sub(block.text.len());
        }
        true
    }

    /// Like [`Feed::place_after_anchor`], but inserts nothing when the block
    /// cannot go under the anchor.
    #[cfg(test)]
    pub fn insert_after_anchor(
        &mut self,
        anchor: &str,
        style: BlockStyle,
        text: impl Into<String>,
    ) -> bool {
        self.anchor_insert_position(anchor).is_some()
            && self.place_after_anchor(anchor, style, text)
    }

    /// Where a block placed under `anchor` goes, or `None` when it cannot.
    fn anchor_insert_position(&self, anchor: &str) -> Option<usize> {
        if anchor.is_empty() {
            return None;
        }
        let anchor_index = self
            .blocks
            .iter()
            .rposition(|block| block.anchor.as_deref() == Some(anchor))?;
        let mut position = anchor_index + 1;
        while self.blocks.get(position).is_some_and(|block| {
            block.anchor.is_none() && block.attached_to.as_deref() == Some(anchor)
        }) {
            position += 1;
        }
        if self.blocks.iter().skip(position).any(|block| block.running) {
            return None;
        }
        Some(position)
    }

    /// Push an empty block that a producer will append to incrementally
    /// (e.g. streaming agent tokens). While running, agent blocks parse
    /// markdown only for completed lines and render the unfinished tail line
    /// as plain text. Call `finalize_block` (or `finalize_last`) when the
    /// stream ends.
    pub fn push_streaming_block(&mut self, style: BlockStyle) {
        self.prune_completed_prefix();
        self.touch(self.blocks.len());
        let mut block = Block::new(style, "");
        block.running = true;
        if let Some(reasoning) = block.reasoning.as_mut() {
            reasoning.started = Some(Instant::now());
            reasoning.elapsed_secs = Some(0);
        }
        if block.reasoning.is_some() {
            self.running_reasoning = Some(self.blocks.len());
        }
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
            self.generation = self.generation.wrapping_add(1);
            mark_dirty(&self.layout_dirty_from, idx);
            block.running = false;
            if self.running_reasoning == Some(idx) {
                self.running_reasoning = None;
            }
            if let Some(reasoning) = block.reasoning.as_mut()
                && let Some(started) = reasoning.started.take()
            {
                reasoning.elapsed_secs = Some(started.elapsed().as_secs());
            }
            // The streamed stable segments are exactly what a whole-text parse
            // produces, so finalizing re-parses only from the last stable
            // boundary (the former tail included) on the next layout. A
            // document-level construct the region split cannot reproduce
            // forces the full re-parse; exact syntax rows remain reusable.
            if finalize_needs_full_parse(block) {
                block.clear_md_cache();
            }
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
    #[cfg(test)]
    pub fn append_to(&mut self, idx: usize, text: impl AsRef<str>) -> bool {
        let text = text.as_ref();
        if has_terminal_control(text) {
            let safe = sanitize_output(text);
            self.append_clean(idx, &safe, || safe.chars().count())
        } else {
            self.append_clean(idx, text, || text.chars().count())
        }
    }

    /// [`Feed::append_to`] for a delta that already went through the
    /// sanitiser: the feed's own control scan and char count are skipped.
    pub fn append_sanitized(&mut self, idx: usize, text: &Sanitized) -> bool {
        self.append_clean(idx, text.as_str(), || text.char_count())
    }

    /// Append terminal-safe `text`; `chars` counts it for a reasoning header.
    fn append_clean(&mut self, idx: usize, text: &str, chars: impl FnOnce() -> usize) -> bool {
        if let Some(block) = self.blocks.get_mut(idx) {
            self.generation = self.generation.wrapping_add(1);
            mark_dirty(&self.layout_dirty_from, idx);
            let old_len = block.text.len();
            if let Some(reasoning) = block.reasoning.as_mut() {
                reasoning.chars = reasoning.chars.saturating_add(chars());
            }
            block.text.push_str(text);
            block.note_appended(text);
            let compacted = compact_oversized_block(&mut block.text);
            self.total_bytes = self
                .total_bytes
                .saturating_sub(old_len)
                .saturating_add(block.text.len());
            block.revision = block.revision.wrapping_add(1);
            *block.render_cache.borrow_mut() = None;
            if compacted {
                block.clear_highlight_budget();
                *block.plain_cache.borrow_mut() = None;
                block.clear_md_cache();
                block.clear_syntax_cache();
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
        let last_index = self.blocks.len().saturating_sub(1);
        self.touch(last_index);
        if let Some(last) = self.blocks.back_mut() {
            self.total_bytes = self.total_bytes.saturating_sub(last.text.len());
            last.style = style;
            last.text = terminal_safe(text.into());
            last.reasoning = (style == BlockStyle::Reasoning).then(|| ReasoningBlock {
                chars: last.text.chars().count(),
                started: None,
                elapsed_secs: None,
                expanded: None,
            });
            compact_oversized_block(&mut last.text);
            self.total_bytes = self.total_bytes.saturating_add(last.text.len());
            last.running = false;
            if self.running_reasoning == Some(last_index) {
                self.running_reasoning = None;
            }
            last.revision = last.revision.wrapping_add(1);
            last.clear_highlight_budget();
            *last.plain_cache.borrow_mut() = None;
            last.clear_md_cache();
            last.clear_syntax_cache();
            *last.render_cache.borrow_mut() = None;
        } else {
            let block = Block::new(style, text);
            self.total_bytes = block.text.len();
            self.blocks.push_back(block);
        }
        self.prune_completed_prefix();
    }

    /// Drop every block from index `len` on (a collapsed `/cron` noop fire
    /// retracts its own echo and output). Anchors live on the blocks
    /// themselves, so nothing else refers to the removed indices.
    pub fn truncate_blocks(&mut self, len: usize) {
        self.touch(len);
        self.total_bytes = self
            .blocks
            .iter()
            .take(len)
            .map(|block| block.text.len())
            .sum();
        self.blocks.truncate(len);
        if self.running_reasoning.is_some_and(|index| index >= len) {
            self.running_reasoning = None;
        }
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
        self.layout(width, None)
    }

    /// Tail-first layout: the rows of the last blocks, at least `min_rows` of
    /// them when the feed has that many, without laying out the history above.
    ///
    /// The result is a suffix of `lines(width)`: indices count from the first
    /// laid-out block, so it is only for painting a bottom-anchored viewport
    /// (`FeedLines::is_complete` says whether earlier rows exist). A later
    /// `lines(width)` keeps these rows and lays out only the remaining
    /// blocks; [`Feed::advance_layout`] does the same in budgeted steps.
    pub(crate) fn lines_tail(&self, width: usize, min_rows: usize) -> Arc<FeedLines> {
        self.layout(width, Some(min_rows))
    }

    /// Lay out up to `max_blocks` more blocks above a tail-first layout at
    /// `width`, e.g. from an idle tick. Returns whether the layout at `width`
    /// is now complete (a later `lines(width)` lays out nothing).
    pub(crate) fn advance_layout(&self, width: usize, max_blocks: usize) -> bool {
        let mut slot = self.layout_cache.borrow_mut();
        let Some(cache) = slot.take() else {
            return false;
        };
        if cache.width != width || cache.generation != self.generation {
            // Stale: the next lines() / lines_tail() call refreshes it first.
            *slot = Some(cache);
            return false;
        }
        if cache.complete() {
            *slot = Some(cache);
            return true;
        }
        let first = cache.first_block.saturating_sub(max_blocks.max(1));
        let cache = self.prepend_blocks(cache, first);
        let complete = cache.complete();
        *slot = Some(cache);
        complete
    }

    fn layout(&self, width: usize, tail_rows: Option<usize>) -> Arc<FeedLines> {
        {
            let cache = self.layout_cache.borrow();
            if let Some(c) = cache.as_ref()
                && c.width == width
                && c.generation == self.generation
                && (c.complete() || tail_rows.is_some_and(|rows| c.lines.len() >= rows))
            {
                return c.lines.clone();
            }
        }
        let dirty_from = self.layout_dirty_from.replace(usize::MAX);
        let previous = self.layout_cache.borrow_mut().take();
        let mut cache = self.compute_lines(width, previous, dirty_from, tail_rows);
        match tail_rows {
            None if !cache.complete() => cache = self.prepend_blocks(cache, 0),
            // A reused tail can be shorter than a taller viewport now needs.
            Some(min_rows) => {
                while !cache.complete() && cache.lines.len() < min_rows {
                    let first = cache.first_block - 1;
                    cache = self.prepend_blocks(cache, first);
                }
            }
            None => {}
        }
        #[cfg(test)]
        self.layout_computes.set(self.layout_computes.get() + 1);
        let lines = Arc::clone(&cache.lines);
        *self.layout_cache.borrow_mut() = Some(cache);
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

    #[cfg(test)]
    pub(crate) fn wrap_bytes(&self) -> usize {
        self.wrap_bytes.get()
    }

    /// Lay out the feed at `width`. Called by `layout` on a cache miss.
    ///
    /// When `previous` was laid out at the same width and only blocks from
    /// `dirty_from` on changed since, its rows for the earlier blocks are kept
    /// (in place when no reader still holds them) and only the changed tail is
    /// laid out again. Without a reusable layout, `tail_rows` asks for a
    /// tail-first layout of at least that many rows instead of every block.
    fn compute_lines(
        &self,
        width: usize,
        previous: Option<LayoutCache>,
        dirty_from: usize,
        tail_rows: Option<usize>,
    ) -> LayoutCache {
        let reusable = previous.filter(|cache| {
            cache.width == width
                && cache.first_block <= self.blocks.len()
                && if dirty_from == usize::MAX {
                    cache.generation == self.generation
                } else {
                    // A change above a partial layout may have shifted the
                    // indices of the blocks it covers.
                    dirty_from >= cache.first_block.max(1)
                }
        });
        let Some(cache) = reusable else {
            return match tail_rows {
                Some(min_rows) => self.tail_layout(width, min_rows),
                None => self.prepend_blocks(
                    LayoutCache {
                        width,
                        generation: self.generation,
                        lines: Arc::new(FeedLines::default()),
                        notice_segments: 0,
                        first_block: self.blocks.len(),
                        block_segments: Vec::new(),
                    },
                    0,
                ),
            };
        };
        let first_block = cache.first_block;
        let keep_blocks = dirty_from
            .saturating_sub(first_block)
            .min(cache.block_segments.len())
            .min(self.blocks.len() - first_block);
        let keep =
            cache.notice_segments + cache.block_segments[..keep_blocks].iter().sum::<usize>();
        let mut lines = match Arc::try_unwrap(cache.lines) {
            Ok(lines) => lines,
            Err(shared) => shared.prefix(keep),
        };
        lines.truncate(keep);
        let mut block_segments = cache.block_segments;
        block_segments.truncate(keep_blocks);
        for block in self.blocks.iter().skip(first_block + keep_blocks) {
            #[cfg(test)]
            self.blocks_laid_out.set(self.blocks_laid_out.get() + 1);
            let before = lines.segments.len();
            push_block_segments(self, block, width, &mut lines);
            block_segments.push(lines.segments.len() - before);
        }
        lines.partial = first_block > 0;
        LayoutCache {
            width,
            generation: self.generation,
            lines: Arc::new(lines),
            notice_segments: cache.notice_segments,
            first_block,
            block_segments,
        }
    }

    /// Lay out blocks from the end until `min_rows` rows exist or every block
    /// is laid out.
    fn tail_layout(&self, width: usize, min_rows: usize) -> LayoutCache {
        let mut first_block = self.blocks.len();
        let mut rows = 0usize;
        let mut reversed: Vec<Vec<Arc<Vec<LineEntry>>>> = Vec::new();
        while first_block > 0 && rows < min_rows {
            first_block -= 1;
            #[cfg(test)]
            self.blocks_laid_out.set(self.blocks_laid_out.get() + 1);
            let segments = block_segments(self, &self.blocks[first_block], width);
            rows += segments.iter().map(|segment| segment.len()).sum::<usize>();
            reversed.push(segments);
        }
        let mut lines = FeedLines::default();
        let mut block_segments = Vec::with_capacity(reversed.len());
        for segments in reversed.into_iter().rev() {
            block_segments.push(segments.len());
            for segment in segments {
                lines.push(segment);
            }
        }
        lines.partial = first_block > 0;
        let cache = LayoutCache {
            width,
            generation: self.generation,
            lines: Arc::new(lines),
            notice_segments: 0,
            first_block,
            block_segments,
        };
        if first_block == 0 {
            // Every block fit: add the omitted-blocks notice to complete it.
            self.prepend_blocks(cache, 0)
        } else {
            cache
        }
    }

    /// Extend a layout upward so it covers `blocks[first..]`, adding the
    /// omitted-blocks notice once it reaches the first block. Keeps every
    /// already laid-out row.
    fn prepend_blocks(&self, cache: LayoutCache, first: usize) -> LayoutCache {
        if first >= cache.first_block {
            return cache;
        }
        let width = cache.width;
        let mut lines = FeedLines::default();
        let mut notice_segments = 0;
        if first == 0 && self.pruned_blocks > 0 {
            lines.push(Arc::new(plain_block_lines(
                &Block::new(
                    BlockStyle::System,
                    format!("[{} earlier feed blocks omitted]", self.pruned_blocks),
                ),
                width,
                self.palette,
            )));
            notice_segments = 1;
        }
        let mut block_segments =
            Vec::with_capacity(cache.first_block - first + cache.block_segments.len());
        for block in self.blocks.range(first..cache.first_block) {
            #[cfg(test)]
            self.blocks_laid_out.set(self.blocks_laid_out.get() + 1);
            let before = lines.segments.len();
            push_block_segments(self, block, width, &mut lines);
            block_segments.push(lines.segments.len() - before);
        }
        block_segments.extend_from_slice(&cache.block_segments);
        let tail = Arc::try_unwrap(cache.lines).unwrap_or_else(|shared| (*shared).clone());
        for segment in tail.segments {
            lines.push(segment);
        }
        lines.partial = first > 0;
        LayoutCache {
            width,
            generation: cache.generation,
            lines: Arc::new(lines),
            notice_segments,
            first_block: first,
            block_segments,
        }
    }

    /// Blocks visited by layout rebuilds so far (test-only).
    #[cfg(test)]
    pub(crate) fn blocks_laid_out(&self) -> usize {
        self.blocks_laid_out.get()
    }

    /// A from-scratch layout through the per-block caches (test-only
    /// reference for the incremental rebuild).
    #[cfg(test)]
    pub(crate) fn full_layout_for_test(&self, width: usize) -> FeedLines {
        let cache = self.compute_lines(width, None, 0, None);
        Arc::try_unwrap(cache.lines).expect("fresh layout is unshared")
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
                    .and_then(|index| self.blocks.remove(index).map(|block| (index, block)))
            } else {
                self.blocks.pop_front().map(|block| (0, block))
            };
            if let Some((index, block)) = block {
                self.note_removed_at(index);
                self.total_bytes = self.total_bytes.saturating_sub(block.text.len());
                self.pruned_blocks = self.pruned_blocks.saturating_add(1);
                removed = true;
            } else {
                break;
            }
        }
        if removed {
            // The omitted-blocks notice and every later index change.
            self.touch(0);
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

/// Strip terminal control sequences from text entering the feed.
///
/// Every feed row is eventually written to the terminal, so this is the one
/// boundary no producer can bypass: callers still sanitise untrusted text
/// themselves (defence in depth), and text without any control character
/// other than `\n` passes through without a copy. The renderer adds its own
/// SGR and OSC 8 sequences only at paint time.
fn terminal_safe(text: String) -> String {
    if has_terminal_control(&text) {
        sanitize_output(&text).into_string()
    } else {
        text
    }
}

/// Whether `text` holds a control character (ESC, a C1 introducer, BEL,
/// `\r`, a tab, ...) or a bidi/zero-width format character that
/// [`sanitize_output`] would remove or rewrite.
fn has_terminal_control(text: &str) -> bool {
    text.chars()
        .any(|c| c != '\n' && super::events::needs_terminal_sanitizing(c))
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

/// Append the block's row segments to `lines`, sharing its cached rows.
fn push_block_segments(feed: &Feed, block: &Block, width: usize, lines: &mut FeedLines) {
    {
        let cache = block.render_cache.borrow();
        if let Some(cache) = cache.as_ref()
            && cache.width == width
            && cache.revision == block.revision
        {
            for segment in &cache.segments {
                lines.push(Arc::clone(segment));
            }
            return;
        }
    }
    for segment in block_segments(feed, block, width) {
        lines.push(segment);
    }
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
        BlockStyle::Reasoning => reasoning_block_segments(feed, block, width),
        _ if block.running => plain_body_segments(feed, block, width),
        _ => vec![Arc::new(plain_block_lines(block, width, feed.palette))],
    };
    *block.render_cache.borrow_mut() = Some(BlockRenderCache {
        width,
        revision: block.revision,
        segments: segments.clone(),
    });
    segments
}

/// A reasoning block as a one-row header segment followed, when expanded, by
/// the incrementally wrapped body. The live clock rebuilds only the header.
fn reasoning_block_segments(feed: &Feed, block: &Block, width: usize) -> Vec<Arc<Vec<LineEntry>>> {
    if feed.reasoning_display == ReasoningDisplay::Off {
        return Vec::new();
    }
    let Some(reasoning) = &block.reasoning else {
        return plain_body_segments(feed, block, width);
    };
    let expanded = reasoning
        .expanded
        .unwrap_or(feed.reasoning_display == ReasoningDisplay::Full);
    let elapsed = reasoning
        .elapsed_secs
        .map(|seconds| format!(", {seconds} s"))
        .unwrap_or_default();
    let header = format!(
        "{} {} ({} chars{})",
        if expanded { "▾" } else { "▸" },
        if block.running {
            "thinking…"
        } else {
            "thought"
        },
        reasoning.chars,
        elapsed
    );
    // A disclosure occupies exactly one visual row at every terminal width.
    let clipped: String = header
        .chars()
        .scan(0usize, |columns, ch| {
            *columns =
                columns.saturating_add(unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0));
            (*columns <= width).then_some(ch)
        })
        .collect();
    let mut segments = vec![Arc::new(vec![LineEntry {
        syntax_spans: None,
        text: CompactString::from(clipped),
        color: feed.palette.reasoning,
    }])];
    if expanded {
        segments.extend(plain_body_segments(feed, block, width));
    }
    segments
}

/// `plain_block_lines` split into shared segments for the completed lines
/// and a fresh segment for the unfinished tail line. Rows for completed lines
/// are memoized per width; an append wraps only the lines it completed.
fn plain_body_segments(feed: &Feed, block: &Block, width: usize) -> Vec<Arc<Vec<LineEntry>>> {
    let color = feed.palette.block_color(block.style);
    let completed_len = block.text.rfind('\n').map_or(0, |idx| idx + 1);
    let mut cache = block.plain_cache.borrow_mut();
    // Text only grows between the mutators that clear this cache, so a cache
    // with a shorter prefix at this width is an append-only extension.
    let (mut segments, start) = match cache.take() {
        Some(cached) if cached.width == width && cached.completed_len <= completed_len => {
            (cached.segments, cached.completed_len)
        }
        _ => (Vec::new(), 0),
    };
    if completed_len > start {
        // Whole lines start..completed_len, without the final newline.
        let region = &block.text[start..completed_len - 1];
        #[cfg(test)]
        feed.wrap_bytes.set(feed.wrap_bytes.get() + region.len());
        let mut rows = Vec::new();
        push_plain_rows(region, width, color, &mut rows);
        push_stable_segment(&mut segments, rows);
    }
    *cache = Some(PlainCache {
        width,
        completed_len,
        segments: segments.clone(),
    });
    let tail = &block.text[completed_len..];
    #[cfg(test)]
    feed.wrap_bytes.set(feed.wrap_bytes.get() + tail.len());
    let mut rows = Vec::new();
    push_plain_rows(tail, width, color, &mut rows);
    segments.push(Arc::new(rows));
    segments
}

fn plain_block_lines(block: &Block, width: usize, palette: ForegroundPalette) -> Vec<LineEntry> {
    let mut result = Vec::new();
    push_plain_rows(
        &block.text,
        width,
        palette.block_color(block.style),
        &mut result,
    );
    result
}

/// One row per empty line and word-wrapped rows per non-empty line of `text`.
fn push_plain_rows(text: &str, width: usize, color: Color, result: &mut Vec<LineEntry>) {
    for line in text.split('\n') {
        let trimmed = line.trim_end_matches('\r');
        if trimmed.is_empty() {
            result.push(LineEntry {
                syntax_spans: None,
                text: CompactString::new(""),
                color,
            });
        } else {
            for chunk in word_wrap(trimmed, width) {
                result.push(LineEntry {
                    syntax_spans: None,
                    text: chunk,
                    color,
                });
            }
        }
    }
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
) -> (usize, Option<OpenFence>, bool) {
    let up_to = up_to.min(text.len());
    let start = start.min(up_to);
    let search_text = &text[start..up_to];

    let mut fence: Option<OpenFence> = entry_fence.cloned();
    let mut in_list = false;
    let mut in_indented_code = false;
    let mut deferred_blank = None;
    let mut last_stable_pos = start;
    // Whether the fence open at `last_stable_pos` is the currently open
    // `fence`. A stable boundary inside a fence is always inside the fence
    // still open (closing a replayable fence moves the boundary past it), so a
    // flag replaces cloning the fence for every code line.
    let mut stable_in_open_fence = fence.is_some();
    // Whether `last_stable_pos` was last set by the forced-window rule.
    let mut stable_forced = false;
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

        if let Some(open) = fence.as_ref() {
            let replayable = open.replayable;
            if fence_closing(line, open) {
                fence = None;
                if replayable {
                    last_stable_pos = line_end;
                    stable_forced = false;
                    stable_in_open_fence = false;
                }
            } else if replayable {
                last_stable_pos = line_end;
                stable_forced = false;
                stable_in_open_fence = true;
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
                stable_forced = false;
                stable_in_open_fence = false;
            }
            byte_pos = line_end;
            continue;
        }

        if let Some(blank_end) = deferred_blank.take() {
            let continues_list = in_list && (is_list_item(trimmed) || indent > 0);
            let continues_indented_code = in_indented_code && indent >= 4;
            if !continues_list && !continues_indented_code {
                last_stable_pos = blank_end;
                stable_forced = false;
                stable_in_open_fence = false;
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
                stable_forced = false;
                stable_in_open_fence = true;
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
            stable_forced = true;
            stable_in_open_fence = false;
            in_list = false;
            in_indented_code = false;
            deferred_blank = None;
        }

        byte_pos = line_end;
    }

    let last_stable_fence = if stable_in_open_fence { fence } else { None };
    (last_stable_pos, last_stable_fence, stable_forced)
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

/// Whether a finalized block's streamed markdown layout can differ from a
/// parse of its whole text, so the stable segments must be discarded:
/// - a link or footnote definition applies to references in other regions,
///   including later ones parsed on their own;
/// - a boundary forced by `MAX_UNSTABLE_WINDOW` split a construct;
/// - an HTML block (a line opening with `<`) such as a comment or `<pre>` may
///   run across the blank lines the boundary scan treats as final.
fn finalize_needs_full_parse(block: &Block) -> bool {
    if block.style != BlockStyle::Agent {
        return false;
    }
    let forced = block.agent.get().is_some_and(|agent| {
        agent
            .md
            .borrow()
            .as_ref()
            .is_some_and(|cache| cache.forced_split)
    });
    forced
        || contains_global_markdown_definition(&block.text)
        || block.text.lines().any(|line| {
            let indent = line.bytes().take_while(|byte| *byte == b' ').count();
            indent <= 3 && line[indent..].starts_with('<')
        })
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
    let tail = agent_tail_lines(block, width, segments.is_empty(), feed.palette);
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
#[allow(clippy::too_many_arguments)]
fn render_region(
    feed: &Feed,
    block: &Block,
    text: &str,
    entry_fence: Option<&OpenFence>,
    entry_code: Option<super::highlight::CodeState>,
    width: usize,
    highlighting: bool,
    ends_inside_fence: bool,
) -> StyledMarkdown {
    // `highlighting` already holds the whole block's budget verdict. A region
    // (with its replayed fence opener) is a subset of the block's lines, so it
    // never exceeds a budget the block fits.
    let mut rendered = match entry_fence {
        Some(fence) => {
            let mut replayed = String::with_capacity(fence.opener.len() + 1 + text.len());
            replayed.push_str(&fence.opener);
            replayed.push('\n');
            replayed.push_str(text);
            parse_agent_markdown(feed, block, &replayed, width, highlighting, entry_code)
        }
        None => parse_agent_markdown(feed, block, text, width, highlighting, None),
    };
    if ends_inside_fence {
        // Closing a code block emits the blank separator row, and the code
        // accumulator's trailing newline emits one empty code row. Both belong
        // to the fence's real close, which has not been seen yet.
        for _ in 0..2 {
            if rendered.lines.last().is_some_and(|row| row.text.is_empty()) {
                rendered.lines.pop();
            }
        }
    }
    if !ends_inside_fence {
        rendered.code_state = None;
    }
    rendered
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

    let highlighting = !feed.monochrome && block.within_highlight_budget();

    // Try an exact cache hit before scanning markdown boundaries. Appending an
    // unfinished tail leaves completed_len unchanged and needs no markdown work.
    if let Some(cached) = cached_agent_lines(block, width, completed_len).filter(|_| {
        block
            .agent_render()
            .md
            .borrow()
            .as_ref()
            .is_some_and(|cache| cache.highlighting == highlighting)
    }) {
        return cached;
    }

    // Mutators that can replace text clear md_cache, so a same-width cache with
    // a shorter parsed prefix is known to be an append-only extension.
    let incremental_base = {
        let cache = block.agent_render().md.borrow();
        cache.as_ref().and_then(|cache| {
            (cache.width == width
                && cache.parsed_len < completed_len
                && cache.highlighting == highlighting)
                .then(|| {
                    (
                        cache.stable_len,
                        cache.stable_fence.clone(),
                        cache.stable_code,
                        cache.stable_segments.clone(),
                        cache.has_rows(),
                        cache.parsed_len,
                        cache.forced_split,
                    )
                })
        })
    };

    // Find the new stable boundary once: it decides both whether the cached
    // prefix is still valid and where the next finalized region ends.
    let incremental = incremental_base.and_then(
        |(
            previous_stable_len,
            previous_fence,
            stable_code,
            stable_segments,
            has_rows,
            previous_parsed_len,
            previous_forced,
        )| {
            let (stable_len, stable_fence, forced) = find_stable_boundary(
                &block.text,
                previous_stable_len,
                completed_len,
                previous_fence.as_ref(),
            );
            // A link or footnote definition can change how an earlier reference
            // renders, so its arrival invalidates the cached prefix. Nothing
            // inside a fence is a definition, so a region that stays inside one
            // never needs the check.
            let stays_in_fence = previous_fence.is_some() && stable_fence == previous_fence;
            (stays_in_fence
                || !contains_global_markdown_definition(
                    &block.text[previous_parsed_len..completed_len],
                ))
            .then_some((
                previous_stable_len,
                previous_fence,
                stable_code,
                stable_segments,
                has_rows,
                stable_len,
                stable_fence,
                previous_forced || (forced && stable_len > previous_stable_len),
            ))
        },
    );

    if let Some((
        previous_stable_len,
        previous_fence,
        mut stable_code,
        mut stable_segments,
        mut has_rows,
        stable_len,
        stable_fence,
        forced_split,
    )) = incremental
    {
        if stable_len > previous_stable_len {
            let mut finalized = render_region(
                feed,
                block,
                &block.text[previous_stable_len..stable_len],
                previous_fence.as_ref(),
                stable_code,
                width,
                highlighting,
                stable_fence.is_some(),
            );
            stable_code = finalized.code_state.take();
            let mut finalized = finalized.lines;
            if !has_rows && !finalized.is_empty() {
                prefix_agent_first_line(&mut finalized);
                has_rows = true;
            }
            push_stable_segment(&mut stable_segments, finalized);
        }

        let suffix = render_region(
            feed,
            block,
            &block.text[stable_len..completed_len],
            stable_fence.as_ref(),
            stable_code,
            width,
            highlighting,
            false,
        );
        let mut suffix = suffix.lines;
        if !has_rows && !suffix.is_empty() {
            prefix_agent_first_line(&mut suffix);
        }

        let cache = MdCache {
            width,
            highlighting,
            stable_code,
            stable_len,
            stable_fence,
            stable_segments,
            parsed_len: completed_len,
            suffix: Arc::new(suffix),
            forced_split,
        };
        let segments = cache.segments();
        *block.agent_render().md.borrow_mut() = Some(cache);
        return segments;
    }

    // Full re-parse: a new stable boundary must be established, or the width
    // changed and every row has to be laid out again.
    let (stable_len, stable_fence, forced_split) =
        find_stable_boundary(&block.text, 0, completed_len, None);
    let mut stable_segments: Vec<Arc<Vec<LineEntry>>> = Vec::new();
    let mut has_rows = false;
    let mut stable_code = None;
    if stable_len > 0 {
        let mut finalized = render_region(
            feed,
            block,
            &block.text[..stable_len],
            None,
            None,
            width,
            highlighting,
            stable_fence.is_some(),
        );
        stable_code = finalized.code_state.take();
        let mut finalized = finalized.lines;
        if !finalized.is_empty() {
            prefix_agent_first_line(&mut finalized);
            has_rows = true;
        }
        stable_segments.push(Arc::new(finalized));
    }
    let suffix = render_region(
        feed,
        block,
        &block.text[stable_len..completed_len],
        stable_fence.as_ref(),
        stable_code,
        width,
        highlighting,
        false,
    );
    let mut suffix = suffix.lines;
    if !has_rows && !suffix.is_empty() {
        prefix_agent_first_line(&mut suffix);
    }

    let cache = MdCache {
        width,
        highlighting,
        stable_code,
        stable_len,
        stable_fence,
        stable_segments,
        parsed_len: completed_len,
        suffix: Arc::new(suffix),
        forced_split,
    };
    let segments = cache.segments();
    *block.agent_render().md.borrow_mut() = Some(cache);
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
        if let Some(spans) = &mut first.syntax_spans {
            for span in Arc::make_mut(spans) {
                span.range.start += 2;
                span.range.end += 2;
            }
        }
    }
}

fn parse_agent_markdown(
    feed: &Feed,
    block: &Block,
    text: &str,
    width: usize,
    highlighting: bool,
    entry_code: Option<super::highlight::CodeState>,
) -> StyledMarkdown {
    #[cfg(test)]
    feed.markdown_bytes_parsed
        .set(feed.markdown_bytes_parsed.get() + text.len());
    let mut rendered = markdown_with_code_state(
        text,
        width,
        feed.palette,
        highlighting,
        entry_code,
        Some(&block.agent_render().syntax),
    );
    for row in &mut rendered.lines {
        row.color = super::markdown::palette_color(row.color, feed.palette);
    }
    rendered
}

fn agent_tail_lines(
    block: &Block,
    width: usize,
    needs_prefix: bool,
    palette: ForegroundPalette,
) -> Vec<LineEntry> {
    if !block.running {
        return Vec::new();
    }
    let completed_len = block.text.rfind('\n').map_or(0, |idx| idx + 1);
    let tail = block.text[completed_len..].trim_end_matches('\r');
    if tail.is_empty() {
        return Vec::new();
    }
    let color = palette.agent;
    let mut lines = word_wrap(tail, width)
        .into_iter()
        .map(|text| LineEntry {
            syntax_spans: None,
            text,
            color,
        })
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
    let cache = block.agent_render().md.borrow();
    let cache = cache.as_ref()?;
    if cache.width == width && cache.parsed_len == parsed_len {
        Some(cache.segments())
    } else {
        None
    }
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    fn texts(feed: &Feed) -> Vec<String> {
        (0..feed.block_count())
            .map(|index| feed.block_text(index).unwrap().to_owned())
            .collect()
    }

    #[test]
    fn anchor_progress_replaces_in_place_and_yields_to_the_result() {
        let mut feed = Feed::new();
        feed.push_anchored_block("call-1", BlockStyle::Tool, "◈ shell");
        feed.push_block(BlockStyle::Agent, "after");
        assert!(feed.set_anchor_progress("call-1", BlockStyle::ToolResult, "a"));
        assert!(feed.set_anchor_progress("call-1", BlockStyle::ToolResult, "a\nb"));
        assert_eq!(texts(&feed), ["◈ shell", "a\nb", "after"]);

        // A result goes directly under its call, above the preview.
        assert!(feed.place_after_anchor("call-1", BlockStyle::ToolResult, "result"));
        assert_eq!(texts(&feed), ["◈ shell", "result", "a\nb", "after"]);
        let before = feed.generation();
        assert!(feed.clear_anchor_progress("call-1"));
        assert!(feed.generation() > before);
        assert_eq!(texts(&feed), ["◈ shell", "result", "after"]);
        assert!(!feed.clear_anchor_progress("call-1"));
        assert!(!feed.set_anchor_progress("missing", BlockStyle::ToolResult, "x"));
        assert!(!feed.set_anchor_progress("", BlockStyle::ToolResult, "x"));
    }

    #[test]
    fn running_reasoning_slot_survives_inserts_removals_and_pruning() {
        let (max_blocks, _, _) = Feed::retention_limits_for_test();
        let mut feed = Feed::new();
        for index in 0..max_blocks - 6 {
            feed.push_block(BlockStyle::System, format!("done {index}"));
        }
        feed.push_streaming_block(BlockStyle::Reasoning);
        feed.append_reasoning("thinking hard\n");
        let start = Instant::now();
        let generation = feed.generation();
        feed.tick_reasoning_at(start + std::time::Duration::from_secs(3));
        assert!(feed.generation() > generation, "the live header updates");
        feed.push_anchored_block("c2", BlockStyle::Tool, "call");
        feed.push_streaming_block(BlockStyle::Agent);
        assert!(!feed.place_after_anchor("c2", BlockStyle::ToolResult, "result"));
        feed.set_anchor_progress("c2", BlockStyle::ToolResult, "progress");
        feed.clear_anchor_progress("c2");
        // Overflow the cap while the reasoning block runs.
        for index in 0..40 {
            feed.push_block(BlockStyle::System, format!("later {index}"));
        }
        feed.tick_reasoning_at(start + std::time::Duration::from_secs(9));
        feed.append_reasoning("more");
        feed.finish_reasoning();
        let reasoning = (0..feed.block_count())
            .find(|index| feed.block_text(*index) == Some("thinking hard\nmore"))
            .expect("the reasoning block is retained");
        assert!(!feed.is_streaming(reasoning));
        // A new reasoning block after pruning is found and finished again.
        feed.finalize_last();
        for index in 0..40 {
            feed.push_block(BlockStyle::System, format!("again {index}"));
        }
        feed.append_reasoning("second");
        feed.tick_reasoning_at(Instant::now() + std::time::Duration::from_secs(2));
        feed.finish_reasoning();
        let second = (0..feed.block_count())
            .find(|index| feed.block_text(*index) == Some("second"))
            .expect("the second reasoning block is retained");
        assert!(!feed.is_streaming(second));
    }

    #[test]
    fn block_header_is_small() {
        assert!(
            std::mem::size_of::<Block>() <= 256,
            "Block is {} bytes",
            std::mem::size_of::<Block>()
        );
    }

    #[test]
    fn grouped_rows_render_like_one_block_per_row() {
        let rows: Vec<(BlockStyle, String)> = (0..200)
            .map(|index| match index % 5 {
                0 => (BlockStyle::DiffHunk, format!("@@ -{index} @@")),
                1 | 2 => (
                    BlockStyle::DiffAdd,
                    format!("+added row {index} {}", "w ".repeat(50)),
                ),
                3 => (BlockStyle::DiffDel, format!("-removed {index}")),
                _ => (BlockStyle::ToolResult, String::new()),
            })
            .collect();
        let build = |grouped: bool| {
            let mut feed = Feed::new();
            feed.push_anchored_block("call-1", BlockStyle::Tool, "call");
            feed.push_anchored_block("call-2", BlockStyle::Tool, "other");
            if grouped {
                feed.place_rows_after_anchor("call-1", rows.clone());
            } else {
                for (style, text) in rows.clone() {
                    feed.place_after_anchor("call-1", style, text);
                }
            }
            feed
        };
        let (grouped, single) = (build(true), build(false));
        assert!(grouped.block_count() < single.block_count());
        assert_eq!(grouped.block_count(), 2 + 160);
        let (a, b) = (grouped.lines(40), single.lines(40));
        assert_eq!(a.len(), b.len());
        for index in 0..a.len() {
            assert_eq!(a[index].text, b[index].text);
            assert_eq!(a[index].color, b[index].color);
        }
        assert_eq!(grouped.block_text(grouped.block_count() - 1), Some("other"));
    }

    #[test]
    fn anchor_progress_never_shifts_a_running_block() {
        let mut feed = Feed::new();
        feed.push_anchored_block("call-1", BlockStyle::Tool, "◈ shell");
        assert!(feed.set_anchor_progress("call-1", BlockStyle::ToolResult, "a"));
        feed.push_streaming_block(BlockStyle::Agent);
        assert!(!feed.clear_anchor_progress("call-1"));
        assert_eq!(feed.block_count(), 3);
        assert!(feed.is_streaming(2));
    }
}
