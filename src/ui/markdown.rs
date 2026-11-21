use compact_str::CompactString;
use crossterm::style::Color;
use pulldown_cmark::{Alignment, Event, Options, Tag, TagEnd};
use smallvec::{SmallVec, smallvec};

use super::renderer::{Cont, LineEntry};
use super::utils::display_width;

/// Translate only colors produced by this parser; callers' explicit colors
/// and the parsed markdown text are retained.
pub(crate) fn palette_color(color: Color, palette: super::feed::ForegroundPalette) -> Color {
    match color {
        Color::White => palette.agent,
        Color::DarkYellow => palette.code,
        Color::DarkGrey => palette.system,
        Color::Cyan | Color::DarkCyan => palette.link,
        _ => color,
    }
}

pub(crate) fn word_wrap(text: &str, max_width: usize) -> SmallVec<[CompactString; 4]> {
    let mut lines: SmallVec<[CompactString; 4]> = SmallVec::new();
    word_wrap_each(text, max_width, |chunk, _| lines.push(chunk));
    lines
}

/// `word_wrap` that also reports how each chunk continues the previous one.
pub(crate) fn word_wrap_flagged(
    text: &str,
    max_width: usize,
) -> SmallVec<[(CompactString, Cont); 4]> {
    let mut lines: SmallVec<[(CompactString, Cont); 4]> = SmallVec::new();
    word_wrap_each(text, max_width, |chunk, cont| lines.push((chunk, cont)));
    lines
}

fn word_wrap_each(text: &str, max_width: usize, mut emit: impl FnMut(CompactString, Cont)) {
    // Display width never exceeds the byte length, so short text fits without
    // being measured.
    if text.is_empty() || max_width == 0 || text.len() <= max_width {
        emit(CompactString::from(text), Cont::None);
        return;
    }
    if display_width(text) <= max_width {
        emit(CompactString::from(text), Cont::None);
        return;
    }

    let emitted = std::cell::Cell::new(false);
    let mut start_cont = Cont::None;
    let mut push = |chunk: CompactString, next: Cont, start_cont: &mut Cont| {
        emit(chunk, *start_cont);
        emitted.set(true);
        *start_cont = next;
    };
    let mut line = String::new();
    let mut line_width: usize = 0;

    // Keep a line's own indentation on its first row (a line that fits keeps
    // it through the fast path above); cap it so the text still gets room.
    let body = text.trim_start_matches(' ');
    let indent = (text.len() - body.len()).min(max_width / 2);
    if indent > 0 && !body.is_empty() {
        line.push_str(&text[..indent]);
        line_width = indent;
    }

    for word in body.split_inclusive(char::is_whitespace) {
        // `split_inclusive` ends each piece at its first whitespace character,
        // so a word carries at most one trailing whitespace character: measure
        // once and subtract that character's width instead of re-measuring.
        let trailing = word.chars().next_back().filter(|c| c.is_whitespace());
        let word_trimmed = match trailing {
            Some(c) => &word[..word.len() - c.len_utf8()],
            None => word,
        };
        let trimmed_w = display_width(word_trimmed);
        let word_w = trimmed_w + display_width(&word[word_trimmed.len()..]);

        if word_trimmed.is_empty() {
            if line_width > 0 && line_width < max_width {
                line.push(' ');
                line_width += 1;
            }
            continue;
        }

        if line_width + word_w <= max_width {
            line.push_str(word);
            line_width += word_w;
        } else if !line.is_empty() && line_width + 1 + trimmed_w <= max_width {
            line.push(' ');
            line.push_str(word_trimmed);
            line_width += 1 + trimmed_w;
            if trailing.is_some() {
                line.push(' ');
                line_width += 1;
            }
        } else {
            if !line.is_empty() {
                push(
                    CompactString::from(line.trim_end()),
                    Cont::Space,
                    &mut start_cont,
                );
            }
            line.clear();
            line_width = 0;

            if trimmed_w > max_width {
                for ch in word_trimmed.chars() {
                    let cw = super::utils::char_display_width(ch);
                    if cw > max_width {
                        if !line.is_empty() {
                            push(CompactString::from(&line), Cont::Split, &mut start_cont);
                            line.clear();
                            line_width = 0;
                        }
                        push(
                            CompactString::from("\u{fffd}"),
                            Cont::Split,
                            &mut start_cont,
                        );
                        continue;
                    }
                    if line_width + cw > max_width && !line.is_empty() {
                        push(CompactString::from(&line), Cont::Split, &mut start_cont);
                        line.clear();
                        line_width = 0;
                    }
                    line.push(ch);
                    line_width += cw;
                }
            } else {
                line.push_str(word_trimmed);
                line_width += trimmed_w;
            }
            if trailing.is_some() {
                line.push(' ');
                line_width += 1;
            }
        }
    }

    let trimmed = line.trim_end();
    if !trimmed.is_empty() {
        push(CompactString::from(trimmed), Cont::Space, &mut start_cont);
    }

    if !emitted.get() {
        push(CompactString::from(text), Cont::Space, &mut start_cont);
    }
}

fn blank_row(color: Color) -> LineEntry {
    LineEntry {
        syntax_spans: None,
        text: CompactString::new(""),
        color,
        theme_accent: false,
        cont: Cont::None,
        lead: 0,
    }
}

fn flush_acc(acc: &str, color: Color, max_width: usize, out: &mut Vec<LineEntry>) {
    if acc.is_empty() {
        return;
    }
    for line in acc.split('\n') {
        let trimmed = line.trim_end_matches('\r');
        if trimmed.is_empty() {
            out.push(blank_row(color));
        } else {
            for (chunk, cont) in word_wrap_flagged(trimmed, max_width) {
                out.push(LineEntry {
                    syntax_spans: None,
                    text: chunk,
                    color,
                    theme_accent: false,
                    cont,
                    lead: 0,
                });
            }
        }
    }
}

/// Flush `acc` as quoted paragraph text: every wrapped row carries the `│ `
/// bar (its two columns are `lead`, so copying skips them).
fn flush_quote(acc: &str, color: Color, max_width: usize, out: &mut Vec<LineEntry>) {
    let width = max_width.saturating_sub(2).max(1);
    for line in acc.split('\n') {
        let trimmed = line.trim_end_matches('\r');
        if trimmed.is_empty() {
            out.push(blank_row(color));
            continue;
        }
        for (chunk, cont) in word_wrap_flagged(trimmed, width) {
            out.push(LineEntry {
                syntax_spans: None,
                text: CompactString::from(format!("│ {chunk}")),
                color,
                theme_accent: false,
                cont,
                lead: "│ ".len() as u8,
            });
        }
    }
}

/// Wrap `line` with `first` as the first row's prefix and a hanging indent of
/// `indent` columns on continuation rows.
fn push_hanging(
    out: &mut Vec<LineEntry>,
    line: &str,
    color: Color,
    max_width: usize,
    first: &str,
    indent: usize,
) {
    let prefix_w = display_width(first).max(indent);
    let avail = if max_width == 0 {
        0
    } else {
        max_width.saturating_sub(prefix_w).max(1)
    };
    for (i, (chunk, cont)) in word_wrap_flagged(line, avail).into_iter().enumerate() {
        let (text, lead) = if i == 0 {
            (format!("{first}{chunk}"), 0)
        } else {
            (
                format!("{:indent$}{chunk}", "", indent = indent),
                indent.min(u8::MAX as usize) as u8,
            )
        };
        out.push(LineEntry {
            syntax_spans: None,
            text: CompactString::from(text),
            color,
            theme_accent: false,
            cont,
            lead,
        });
    }
}

struct ListFrame {
    ordered: bool,
    next: u64,
    /// Hanging indent of the item currently open in this list.
    indent: usize,
}

/// Nested list state: one frame per open list, the pending marker prefix of
/// the current item (taken by the first text flushed for it), and the hanging
/// indent of the current item's text.
#[derive(Default)]
struct ListCtx {
    frames: Vec<ListFrame>,
    marker: Option<String>,
    indent: usize,
}

impl ListCtx {
    fn start_item(&mut self, color: Color) {
        let depth = self.frames.len();
        let Some(frame) = self.frames.last_mut() else {
            return;
        };
        let mut prefix = "  ".repeat(depth.saturating_sub(1));
        if frame.ordered {
            use std::fmt::Write as _;
            let _ = write!(prefix, " {}. ", frame.next);
            frame.next += 1;
        } else {
            prefix.push_str(bullet_prefix(color));
        }
        self.indent = display_width(&prefix);
        frame.indent = self.indent;
        self.marker = Some(prefix);
    }
}

/// Flush `acc` as paragraph text. Inside a list the first row of the item
/// carries its marker and the other rows hang under the item text.
fn flush_text(
    acc: &str,
    color: Color,
    max_width: usize,
    lists: &mut ListCtx,
    out: &mut Vec<LineEntry>,
) {
    if acc.is_empty() {
        return;
    }
    if lists.frames.is_empty() {
        flush_acc(acc, color, max_width, out);
        return;
    }
    let indent = lists.indent;
    let mut first = lists.marker.take();
    for line in acc.split('\n') {
        let trimmed = line.trim_end_matches('\r');
        if trimmed.is_empty() {
            out.push(blank_row(color));
            continue;
        }
        let prefix = first.take().unwrap_or_else(|| " ".repeat(indent));
        push_hanging(out, trimmed, color, max_width, &prefix, indent);
    }
}

/// Emit the `↪ url` rows collected for links in the paragraph just flushed.
fn flush_links(links: &mut Vec<String>, max_width: usize, out: &mut Vec<LineEntry>) {
    for url in links.drain(..) {
        push_hanging(out, &url, Color::DarkGrey, max_width, "  ↪ ", 4);
    }
}

/// Hard-wrap one code row at `max_width` display columns (no whitespace
/// collapsing), splitting its syntax spans at the cuts.
fn push_code_rows(
    out: &mut Vec<LineEntry>,
    text: &str,
    spans: Option<std::sync::Arc<[super::highlight::TokenSpan]>>,
    max_width: usize,
) {
    let mut push = |range: std::ops::Range<usize>, cont: Cont, whole: bool| {
        let syntax_spans = match (&spans, whole) {
            (Some(spans), true) => Some(spans.clone()),
            (Some(spans), false) => {
                let clipped: Vec<_> = spans
                    .iter()
                    .filter(|span| span.range.start < range.end && span.range.end > range.start)
                    .map(|span| super::highlight::TokenSpan {
                        range: span.range.start.max(range.start) - range.start
                            ..span.range.end.min(range.end) - range.start,
                        color: span.color,
                        attrs: span.attrs,
                    })
                    .collect();
                Some(clipped.into()).filter(|c: &std::sync::Arc<[_]>| !c.is_empty())
            }
            (None, _) => None,
        };
        out.push(LineEntry {
            text: CompactString::from(&text[range]),
            color: Color::DarkYellow,
            syntax_spans,
            theme_accent: false,
            cont,
            lead: 0,
        });
    };
    if max_width == 0 || text.len() <= max_width || display_width(text) <= max_width {
        push(0..text.len(), Cont::None, true);
        return;
    }
    let mut start = 0;
    let mut width = 0;
    let mut cont = Cont::None;
    for (idx, ch) in text.char_indices() {
        let cw = super::utils::char_display_width(ch);
        if width + cw > max_width && idx > start {
            push(start..idx, cont, false);
            cont = Cont::Split;
            start = idx;
            width = 0;
        }
        width += cw;
    }
    push(start..text.len(), cont, false);
}

/// `text.trim()` without reallocating: the leading whitespace is drained and
/// the trailing whitespace truncated in place.
fn trim_in_place(mut text: String) -> String {
    let end = text.trim_end().len();
    text.truncate(end);
    let start = text.len() - text.trim_start().len();
    text.drain(..start);
    text
}

fn bullet_prefix(col: Color) -> &'static str {
    match col {
        Color::DarkGrey => "  ┊ ",
        _ => "  • ",
    }
}

#[cfg(test)]
pub fn markdown_to_styled(text: &str, max_width: usize) -> Vec<LineEntry> {
    markdown_with_code_state(
        text,
        max_width,
        super::feed::ForegroundPalette::default(),
        super::highlight::within_budget(text),
        None,
        None,
    )
    .lines
}

pub(crate) struct StyledMarkdown {
    pub lines: Vec<LineEntry>,
    pub code_state: Option<super::highlight::CodeState>,
}

/// Lay out `text` as styled markdown rows. `highlighting` must already include
/// the highlight budget verdict ([`super::highlight::within_budget`]) for
/// `text` or for a document `text` is a line-aligned part of; it is not
/// re-checked here, so streaming callers do not rescan the block per region.
pub(crate) fn markdown_with_code_state(
    text: &str,
    max_width: usize,
    palette: super::feed::ForegroundPalette,
    highlighting: bool,
    mut entry_code_state: Option<super::highlight::CodeState>,
    token_cache: Option<&std::cell::RefCell<super::highlight::TokenCache>>,
) -> StyledMarkdown {
    if text.is_empty() {
        return StyledMarkdown {
            lines: Vec::new(),
            code_state: entry_code_state,
        };
    }
    let mut code_state = None;
    let mut last_code_state = None;

    let parser = pulldown_cmark::Parser::new_ext(
        text,
        Options::ENABLE_TABLES
            | Options::ENABLE_FOOTNOTES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS,
    );
    // Roughly one row per 40 source bytes; avoids repeated regrowth.
    let mut result = Vec::with_capacity(text.len() / 40);
    let mut acc = String::new();
    // Reused for prefixed rows (blockquote bar, list bullet, link note).
    let mut prefixed = String::new();

    let mut in_heading = false;
    let mut in_code_block = false;
    let mut in_blockquote = false;
    let mut lists = ListCtx::default();
    let mut pending_links: Vec<String> = Vec::new();
    // Byte range of the link text in `acc`, to colour a paragraph that is one link.
    // Whether the current paragraph holds link text and text outside a link,
    // to colour a paragraph that is only a link as a whole row.
    let mut para_link = false;
    let mut para_other = false;
    let mut in_link = false;
    let mut heading_level: Option<pulldown_cmark::HeadingLevel> = None;
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    let mut table_row: Vec<String> = Vec::new();
    let mut table_cell = String::new();
    let mut table_alignments: Vec<Alignment> = Vec::new();
    let mut link_url = String::new();
    let mut in_table_cell = false;

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { level, .. } => {
                    flush_text(&acc, Color::White, max_width, &mut lists, &mut result);
                    acc.clear();
                    in_heading = true;
                    // Heading levels differ by attribute: h1 bold and
                    // underlined, h2 bold, the rest italic.
                    match level {
                        pulldown_cmark::HeadingLevel::H1 => {
                            acc.push(BOLD_ON);
                            acc.push(UNDER_ON);
                        }
                        pulldown_cmark::HeadingLevel::H2 => acc.push(BOLD_ON),
                        _ => acc.push(ITAL_ON),
                    }
                    heading_level = Some(level);
                }
                Tag::Emphasis if !in_table_cell => acc.push(ITAL_ON),
                Tag::Strong if !in_table_cell => acc.push(BOLD_ON),
                Tag::Strikethrough if !in_table_cell => acc.push(STRIKE_ON),
                Tag::CodeBlock(kind) => {
                    flush_text(&acc, Color::White, max_width, &mut lists, &mut result);
                    acc.clear();
                    code_state = if highlighting {
                        match kind {
                            pulldown_cmark::CodeBlockKind::Fenced(language) => entry_code_state
                                .take()
                                .or_else(|| super::highlight::CodeState::new(&language)),
                            pulldown_cmark::CodeBlockKind::Indented => None,
                        }
                    } else {
                        None
                    };
                    in_code_block = true;
                }
                Tag::BlockQuote(_) => {
                    flush_text(&acc, Color::White, max_width, &mut lists, &mut result);
                    acc.clear();
                    in_blockquote = true;
                }
                Tag::List(start) => {
                    let color = if in_blockquote {
                        Color::DarkGrey
                    } else {
                        Color::White
                    };
                    flush_text(&acc, color, max_width, &mut lists, &mut result);
                    acc.clear();
                    lists.frames.push(ListFrame {
                        ordered: start.is_some(),
                        next: start.unwrap_or(1),
                        indent: 0,
                    });
                }
                Tag::Item => {
                    let color = if in_blockquote {
                        Color::DarkGrey
                    } else {
                        Color::White
                    };
                    flush_text(&acc, color, max_width, &mut lists, &mut result);
                    acc.clear();
                    lists.start_item(color);
                }
                Tag::FootnoteDefinition(_) => {}
                Tag::Table(alignments) => {
                    flush_text(&acc, Color::White, max_width, &mut lists, &mut result);
                    acc.clear();
                    table_rows.clear();
                    table_row.clear();
                    table_cell.clear();
                    table_alignments = alignments;
                }
                Tag::TableHead => {
                    table_rows.clear();
                }
                Tag::TableRow => {
                    table_row.clear();
                }
                Tag::TableCell => {
                    table_cell.clear();
                    in_table_cell = true;
                }
                Tag::Link { dest_url, .. } => {
                    link_url.clear();
                    link_url.push_str(&dest_url);
                    if !in_table_cell {
                        acc.push(LINK_ON);
                        in_link = true;
                        para_link = true;
                    }
                }
                Tag::Image { dest_url, .. } => {
                    link_url.clear();
                    link_url.push_str(&dest_url);
                }
                _ => {}
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::Paragraph => {
                    let color = if in_blockquote {
                        Color::DarkGrey
                    } else if lists.frames.is_empty() && para_link && !para_other {
                        Color::DarkCyan
                    } else {
                        Color::White
                    };
                    para_link = false;
                    para_other = false;
                    if in_blockquote && lists.frames.is_empty() {
                        flush_quote(&acc, color, max_width, &mut result);
                    } else {
                        flush_text(&acc, color, max_width, &mut lists, &mut result);
                    }
                    acc.clear();
                    flush_links(&mut pending_links, max_width, &mut result);
                }
                TagEnd::Heading(_) => {
                    match heading_level.take() {
                        Some(pulldown_cmark::HeadingLevel::H1) => {
                            acc.push(UNDER_OFF);
                            acc.push(BOLD_OFF);
                        }
                        Some(pulldown_cmark::HeadingLevel::H2) => acc.push(BOLD_OFF),
                        _ => acc.push(ITAL_OFF),
                    }
                    flush_acc(&acc, Color::Cyan, max_width, &mut result);
                    acc.clear();
                    para_link = false;
                    para_other = false;
                    flush_links(&mut pending_links, max_width, &mut result);
                    in_heading = false;
                    result.push(LineEntry {
                        syntax_spans: None,
                        text: CompactString::new(""),
                        color: Color::White,
                        theme_accent: false,
                        cont: Cont::None,
                        lead: 0,
                    });
                }
                TagEnd::CodeBlock => {
                    for line in acc.split_inclusive('\n') {
                        let trimmed = line
                            .strip_suffix('\n')
                            .unwrap_or(line)
                            .trim_end_matches('\r');
                        let syntax_spans =
                            code_state.as_mut().and_then(|state| match token_cache {
                                Some(cache) => {
                                    state.cached_line(line, palette, &mut cache.borrow_mut())
                                }
                                None => state.line(line, palette),
                            });
                        push_code_rows(&mut result, trimmed, syntax_spans, max_width);
                    }
                    if acc.is_empty() || acc.ends_with('\n') {
                        result.push(LineEntry {
                            text: CompactString::new(""),
                            color: Color::DarkYellow,
                            syntax_spans: None,
                            theme_accent: false,
                            cont: Cont::None,
                            lead: 0,
                        });
                    }
                    last_code_state = code_state.take();
                    acc.clear();
                    in_code_block = false;
                    result.push(LineEntry {
                        text: CompactString::new(""),
                        color: Color::White,
                        syntax_spans: None,
                        theme_accent: false,
                        cont: Cont::None,
                        lead: 0,
                    });
                }
                TagEnd::BlockQuote(_) => {
                    let mut quoted = Vec::new();
                    for line in acc.split('\n') {
                        let trimmed = line.trim_end_matches('\r');
                        if trimmed.is_empty() {
                            quoted.push(LineEntry {
                                syntax_spans: None,
                                text: CompactString::new(""),
                                color: Color::DarkGrey,
                                theme_accent: false,
                                cont: Cont::None,
                                lead: 0,
                            });
                        } else {
                            prefixed.clear();
                            prefixed.push_str("│ ");
                            prefixed.push_str(trimmed);
                            for chunk in word_wrap(&prefixed, max_width) {
                                quoted.push(LineEntry {
                                    syntax_spans: None,
                                    text: chunk,
                                    color: Color::DarkGrey,
                                    theme_accent: false,
                                    cont: Cont::None,
                                    lead: 0,
                                });
                            }
                        }
                    }
                    result.extend(quoted);
                    acc.clear();
                    in_blockquote = false;
                    result.push(LineEntry {
                        syntax_spans: None,
                        text: CompactString::new(""),
                        color: Color::White,
                        theme_accent: false,
                        cont: Cont::None,
                        lead: 0,
                    });
                }
                TagEnd::Item => {
                    let color = if in_blockquote {
                        Color::DarkGrey
                    } else {
                        Color::White
                    };
                    flush_text(&acc, color, max_width, &mut lists, &mut result);
                    acc.clear();
                    para_link = false;
                    para_other = false;
                    flush_links(&mut pending_links, max_width, &mut result);
                    lists.marker = None;
                }
                TagEnd::List(_) => {
                    lists.frames.pop();
                    // A nested list's parent item keeps its hanging indent.
                    lists.indent = lists.frames.last().map_or(0, |frame| frame.indent);
                    if lists.frames.is_empty() {
                        result.push(blank_row(Color::White));
                    }
                }
                TagEnd::Emphasis if !in_table_cell => acc.push(ITAL_OFF),
                TagEnd::Strong if !in_table_cell => acc.push(BOLD_OFF),
                TagEnd::Strikethrough if !in_table_cell => acc.push(STRIKE_OFF),
                TagEnd::Link | TagEnd::Image => {
                    if in_link && matches!(tag_end, TagEnd::Link) {
                        acc.push(LINK_OFF);
                        in_link = false;
                    }
                    if !link_url.is_empty() {
                        if in_table_cell {
                            table_cell.push_str(" (");
                            table_cell.push_str(&link_url);
                            table_cell.push(')');
                        } else {
                            pending_links.push(link_url.clone());
                        }
                    }
                    link_url.clear();
                }
                TagEnd::Table => {
                    flush_table(&table_rows, &table_alignments, max_width, &mut result);
                    table_rows.clear();
                    table_alignments.clear();
                    result.push(LineEntry {
                        syntax_spans: None,
                        text: CompactString::new(""),
                        color: Color::White,
                        theme_accent: false,
                        cont: Cont::None,
                        lead: 0,
                    });
                }
                TagEnd::TableHead => {
                    let cells = std::mem::take(&mut table_row);
                    let cell_text: Vec<String> = cells.into_iter().map(trim_in_place).collect();
                    if !cell_text.iter().all(|c| c.is_empty()) {
                        table_rows.push(cell_text);
                    }
                }
                TagEnd::TableRow => {
                    let cells = std::mem::take(&mut table_row);
                    let cell_text: Vec<String> = cells.into_iter().map(trim_in_place).collect();
                    if !cell_text.iter().all(|c| c.is_empty()) {
                        table_rows.push(cell_text);
                    }
                }
                TagEnd::TableCell => {
                    in_table_cell = false;
                    table_row.push(std::mem::take(&mut table_cell));
                }
                TagEnd::FootnoteDefinition => {}
                _ => {}
            },
            Event::Text(t) => {
                if in_table_cell {
                    table_cell.push_str(&t);
                } else {
                    if !in_link {
                        para_other = true;
                    }
                    // Layout marks are private to this module: never let
                    // model text forge one.
                    if t.chars().any(is_mark) {
                        acc.extend(t.chars().filter(|c| !is_mark(*c)));
                    } else {
                        acc.push_str(&t);
                    }
                }
            }
            Event::Code(t) => {
                if !in_link {
                    para_other = true;
                }
                let target = if in_table_cell {
                    &mut table_cell
                } else {
                    &mut acc
                };
                target.push('`');
                target.push_str(&t);
                target.push('`');
            }
            Event::SoftBreak | Event::HardBreak => {
                if in_table_cell {
                    table_cell.push('\n');
                } else {
                    acc.push('\n');
                }
            }
            Event::Rule => {
                flush_acc(&acc, Color::White, max_width, &mut result);
                acc.clear();
                let rule: String = "\u{2500}".repeat(max_width.min(40));
                result.push(LineEntry {
                    syntax_spans: None,
                    text: CompactString::from(rule),
                    color: Color::DarkGrey,
                    theme_accent: false,
                    cont: Cont::None,
                    lead: 0,
                });
                result.push(LineEntry {
                    syntax_spans: None,
                    text: CompactString::new(""),
                    color: Color::White,
                    theme_accent: false,
                    cont: Cont::None,
                    lead: 0,
                });
            }
            Event::Html(t) => {
                if in_table_cell {
                    table_cell.push_str(&t);
                } else {
                    acc.push_str(&t);
                }
            }
            Event::InlineHtml(t) => {
                if in_table_cell {
                    table_cell.push_str(&t);
                } else {
                    acc.push_str(&t);
                }
            }
            Event::FootnoteReference(t) => {
                acc.push_str(&t);
            }
            Event::TaskListMarker(checked) => {
                if checked {
                    acc.push_str("[x]");
                } else {
                    acc.push_str("[ ]");
                }
            }
            _ => {}
        }
    }

    if !acc.is_empty() {
        let color = if in_blockquote {
            Color::DarkGrey
        } else if in_code_block {
            Color::DarkYellow
        } else if in_heading {
            Color::Cyan
        } else {
            Color::White
        };
        flush_acc(&acc, color, max_width, &mut result);
    }

    apply_marks(&mut result, palette);
    StyledMarkdown {
        lines: result,
        code_state: last_code_state,
    }
}

// Zero-width layout marks carried inside `acc` through wrapping, then turned
// into attribute spans by `apply_marks`. They are zero columns wide, so
// wrapping measures the visible text only.
const BOLD_ON: char = '\u{2061}';
const BOLD_OFF: char = '\u{2062}';
const ITAL_ON: char = '\u{2063}';
const ITAL_OFF: char = '\u{2064}';
const STRIKE_ON: char = '\u{206A}';
const STRIKE_OFF: char = '\u{206B}';
const LINK_ON: char = '\u{206C}';
const LINK_OFF: char = '\u{206D}';
const UNDER_ON: char = '\u{206E}';
const UNDER_OFF: char = '\u{206F}';

fn is_mark(c: char) -> bool {
    matches!(c, '\u{2061}'..='\u{2064}' | '\u{206A}'..='\u{206F}')
}

/// Replace layout marks in `rows` with attribute spans (bold, italic,
/// strikethrough, underline and link colour), carrying the open state across
/// wrapped rows.
fn apply_marks(rows: &mut [LineEntry], palette: super::feed::ForegroundPalette) {
    use super::highlight::{ATTR_BOLD, ATTR_ITALIC, ATTR_STRIKE, ATTR_UNDERLINE, TokenSpan};
    // bold, italic, strike, underline, link
    let mut open = [0i32; 5];
    for row in rows {
        let active = open.iter().any(|count| *count > 0);
        if !active && (!row.text.bytes().any(|b| b == 0xE2) || !row.text.chars().any(is_mark)) {
            continue;
        }
        let state = |open: &[i32; 5]| {
            let mut attrs = 0u8;
            for (count, bit) in
                open[..4]
                    .iter()
                    .zip([ATTR_BOLD, ATTR_ITALIC, ATTR_STRIKE, ATTR_UNDERLINE])
            {
                if *count > 0 {
                    attrs |= bit;
                }
            }
            (attrs, open[4] > 0)
        };
        let mut clean = String::with_capacity(row.text.len());
        let mut spans: Vec<TokenSpan> = Vec::new();
        let mut seg_start = 0;
        let mut current = state(&open);
        let close = |spans: &mut Vec<TokenSpan>, start: usize, end: usize, cur: (u8, bool)| {
            if (cur.0 != 0 || cur.1) && end > start {
                spans.push(TokenSpan {
                    range: start..end,
                    color: if cur.1 { palette.link } else { Color::Reset },
                    attrs: cur.0,
                });
            }
        };
        for c in row.text.chars() {
            if is_mark(c) {
                close(&mut spans, seg_start, clean.len(), current);
                match c {
                    BOLD_ON => open[0] += 1,
                    BOLD_OFF => open[0] = (open[0] - 1).max(0),
                    ITAL_ON => open[1] += 1,
                    ITAL_OFF => open[1] = (open[1] - 1).max(0),
                    STRIKE_ON => open[2] += 1,
                    STRIKE_OFF => open[2] = (open[2] - 1).max(0),
                    UNDER_ON => open[3] += 1,
                    UNDER_OFF => open[3] = (open[3] - 1).max(0),
                    LINK_ON => open[4] += 1,
                    _ => open[4] = (open[4] - 1).max(0),
                }
                seg_start = clean.len();
                current = state(&open);
            } else {
                clean.push(c);
            }
        }
        close(&mut spans, seg_start, clean.len(), current);
        row.text = CompactString::from(clean);
        if row.syntax_spans.is_none() && !spans.is_empty() {
            row.syntax_spans = Some(spans.into());
        }
    }
}

fn flush_table(
    rows: &[Vec<String>],
    alignments: &[Alignment],
    max_width: usize,
    out: &mut Vec<LineEntry>,
) {
    if rows.is_empty() {
        return;
    }

    let col_count = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if col_count == 0 {
        return;
    }

    let mut col_widths: Vec<usize> = vec![0; col_count];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < col_count {
                col_widths[i] = col_widths[i].max(display_width(cell));
            }
        }
    }

    let overhead = 3 * col_count + 1;
    let available = max_width.saturating_sub(overhead);
    const MIN_COL_WIDTH: usize = 4;
    if max_width == 0 {
        return;
    }
    if available < col_count.saturating_mul(MIN_COL_WIDTH) {
        flush_narrow_table(rows, max_width, out);
        return;
    }

    let mut total_req: usize = col_widths.iter().sum();
    while total_req > available {
        let mut widest_idx = 0;
        let mut widest_w = 0;
        for (i, w) in col_widths.iter().enumerate() {
            if *w > widest_w {
                widest_w = *w;
                widest_idx = i;
            }
        }
        if widest_w <= MIN_COL_WIDTH {
            break;
        }
        col_widths[widest_idx] -= 1;
        total_req -= 1;
    }

    let top = format_table_rule(&col_widths, '\u{250c}', '\u{252c}', '\u{2510}');
    let sep = format_table_rule(&col_widths, '\u{251c}', '\u{253c}', '\u{2524}');
    let bot = format_table_rule(&col_widths, '\u{2514}', '\u{2534}', '\u{2518}');

    push_table_line(&top, Color::DarkGrey, out);
    for (i, row) in rows.iter().enumerate() {
        for line in format_table_row(row, &col_widths, alignments) {
            push_table_line(&line, Color::White, out);
        }
        if i == 0 && rows.len() > 1 {
            push_table_line(&sep, Color::DarkGrey, out);
        }
    }
    push_table_line(&bot, Color::DarkGrey, out);
}

fn flush_narrow_table(rows: &[Vec<String>], max_width: usize, out: &mut Vec<LineEntry>) {
    for row in rows {
        let text = row.join(" | ");
        for line in word_wrap(&text, max_width) {
            push_table_line(&line, Color::White, out);
        }
    }
}

fn format_table_rule(widths: &[usize], left: char, mid: char, right: char) -> String {
    let mut s = String::with_capacity(widths.iter().sum::<usize>() + widths.len() * 3);
    s.push(left);
    for (i, w) in widths.iter().enumerate() {
        for _ in 0..*w + 2 {
            s.push('\u{2500}');
        }
        if i + 1 < widths.len() {
            s.push(mid);
        }
    }
    s.push(right);
    s
}

fn push_table_line(text: &str, color: Color, out: &mut Vec<LineEntry>) {
    out.push(LineEntry {
        syntax_spans: None,
        text: CompactString::from(text),
        color,
        theme_accent: false,
        cont: Cont::None,
        lead: 0,
    });
}

fn format_table_row(cells: &[String], widths: &[usize], alignments: &[Alignment]) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cell_wrapped: Vec<SmallVec<[std::borrow::Cow<'_, str>; 1]>> =
        Vec::with_capacity(cells.len());
    let mut max_subrows = 0usize;

    for (i, cell) in cells.iter().enumerate() {
        let width = widths.get(i).copied().unwrap_or(10);
        let wrapped: SmallVec<[std::borrow::Cow<'_, str>; 1]> = if display_width(cell) <= width {
            smallvec![std::borrow::Cow::Borrowed(cell.as_str())]
        } else {
            word_wrap(cell, width)
                .into_iter()
                .map(|chunk| std::borrow::Cow::Owned(chunk.into_string()))
                .collect()
        };
        max_subrows = max_subrows.max(wrapped.len());
        cell_wrapped.push(wrapped);
    }

    for subrow in 0..max_subrows {
        let mut line = String::new();
        line.push('\u{2502}');
        for (i, cw) in cell_wrapped.iter().enumerate() {
            let width = widths.get(i).copied().unwrap_or(10);
            let text = cw.get(subrow).map(|s| s.as_ref()).unwrap_or("");
            let text_w = display_width(text);
            let align = alignments.get(i).copied().unwrap_or(Alignment::None);
            let padding = width.saturating_sub(text_w);
            line.push(' ');
            match align {
                Alignment::Center => {
                    let left_pad = padding / 2;
                    let right_pad = padding - left_pad;
                    for _ in 0..left_pad {
                        line.push(' ');
                    }
                    line.push_str(text);
                    for _ in 0..right_pad {
                        line.push(' ');
                    }
                }
                Alignment::Right => {
                    for _ in 0..padding {
                        line.push(' ');
                    }
                    line.push_str(text);
                }
                Alignment::None | Alignment::Left => {
                    line.push_str(text);
                    for _ in 0..padding {
                        line.push(' ');
                    }
                }
            }
            line.push(' ');
            if i + 1 < cell_wrapped.len() {
                line.push('\u{2502}');
            }
        }
        line.push('\u{2502}');
        lines.push(line);
    }

    lines
}

#[cfg(test)]
mod narrow_table_tests {
    use super::*;

    #[test]
    fn narrow_tables_are_wrapped_instead_of_dropped_or_overflowing() {
        let rows = vec![
            vec!["header one".into(), "header two".into()],
            vec!["界界界".into(), "value".into()],
        ];
        let mut out = Vec::new();
        flush_table(&rows, &[], 7, &mut out);
        assert!(!out.is_empty());
        assert!(out.iter().all(|line| display_width(&line.text) <= 7));
    }

    #[test]
    fn a_trailing_whitespace_width_adds_to_the_word_width() {
        // word_wrap measures a word once and adds its single trailing
        // whitespace character; that is exact only if no whitespace joins a
        // width sequence with the character before it.
        let whitespace = (0u32..=0x3000)
            .filter_map(char::from_u32)
            .filter(|c| c.is_whitespace());
        for ws in whitespace {
            for prefix in [
                "a",
                "雪",
                "😀",
                "\u{644}",
                "e\u{301}",
                "\u{2764}\u{fe0f}",
                "x\u{200d}",
            ] {
                let word = format!("{prefix}{ws}");
                assert_eq!(
                    display_width(&word),
                    display_width(prefix) + display_width(&ws.to_string()),
                    "{word:?}"
                );
            }
        }
    }

    #[test]
    fn a_double_width_glyph_does_not_overflow_a_one_column_wrap() {
        let lines = word_wrap("界", 1);
        assert!(lines.iter().all(|line| display_width(line) <= 1));
    }

    #[test]
    fn a_wrapped_line_keeps_its_first_row_indentation() {
        let lines = word_wrap("    alpha beta gamma delta", 15);
        assert_eq!(lines[0].as_str(), "    alpha beta");
        assert!(lines.len() >= 2);
        // Indentation wider than half the width is capped, never the whole row.
        let lines = word_wrap("                    word another", 10);
        assert!(lines.iter().all(|l| display_width(l) <= 10));
        assert!(lines.iter().any(|l| l.contains("another")));
    }

    #[test]
    fn blockquote_paragraph_rows_all_carry_the_quote_bar() {
        let lines = markdown_to_styled("> alpha beta gamma delta epsilon zeta eta theta", 16);
        let rows: Vec<&str> = lines
            .iter()
            .map(|l| l.text.as_str())
            .filter(|t| !t.is_empty())
            .collect();
        assert!(rows.len() >= 2, "{rows:?}");
        assert!(rows.iter().all(|r| r.starts_with("│ ")), "{rows:?}");
        assert!(rows.iter().all(|r| display_width(r) <= 16), "{rows:?}");
    }
}
