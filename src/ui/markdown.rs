use compact_str::CompactString;
use crossterm::style::Color;
use pulldown_cmark::{Alignment, Event, Options, Tag, TagEnd};
use smallvec::{SmallVec, smallvec};

use super::renderer::LineEntry;
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
    // Display width never exceeds the byte length, so short text fits without
    // being measured.
    if text.is_empty() || max_width == 0 || text.len() <= max_width {
        return smallvec![CompactString::from(text)];
    }
    if display_width(text) <= max_width {
        return smallvec![CompactString::from(text)];
    }

    let mut lines: SmallVec<[CompactString; 4]> = SmallVec::new();
    let mut line = String::new();
    let mut line_width: usize = 0;

    for word in text.split_inclusive(char::is_whitespace) {
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
                lines.push(CompactString::from(line.trim_end()));
            }
            line.clear();
            line_width = 0;

            if trimmed_w > max_width {
                for ch in word_trimmed.chars() {
                    let cw = super::utils::char_display_width(ch);
                    if cw > max_width {
                        if !line.is_empty() {
                            lines.push(CompactString::from(&line));
                            line.clear();
                            line_width = 0;
                        }
                        lines.push(CompactString::from("\u{fffd}"));
                        continue;
                    }
                    if line_width + cw > max_width && !line.is_empty() {
                        lines.push(CompactString::from(&line));
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
        lines.push(CompactString::from(trimmed));
    }

    if lines.is_empty() {
        lines.push(CompactString::from(text));
    }

    lines
}

fn flush_acc(acc: &str, color: Color, max_width: usize, out: &mut Vec<LineEntry>) {
    if acc.is_empty() {
        return;
    }
    for line in acc.split('\n') {
        let trimmed = line.trim_end_matches('\r');
        if trimmed.is_empty() {
            out.push(LineEntry {
                syntax_spans: None,
                text: CompactString::new(""),
                color,
                theme_accent: false,
            });
        } else {
            for chunk in word_wrap(trimmed, max_width) {
                out.push(LineEntry {
                    syntax_spans: None,
                    text: chunk,
                    color,
                    theme_accent: false,
                });
            }
        }
    }
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
    let mut ordered_list = false;
    let mut list_item_count: u64 = 0;
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
                Tag::Heading { .. } => {
                    flush_acc(&acc, Color::White, max_width, &mut result);
                    acc.clear();
                    in_heading = true;
                }
                Tag::CodeBlock(kind) => {
                    flush_acc(&acc, Color::White, max_width, &mut result);
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
                    flush_acc(&acc, Color::White, max_width, &mut result);
                    acc.clear();
                    in_blockquote = true;
                }
                Tag::List(t) => {
                    ordered_list = t.is_some();
                    list_item_count = 0;
                }
                Tag::Item => {
                    flush_acc(&acc, Color::White, max_width, &mut result);
                    acc.clear();
                    list_item_count += 1;
                }
                Tag::FootnoteDefinition(_) => {}
                Tag::Table(alignments) => {
                    flush_acc(&acc, Color::White, max_width, &mut result);
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
                Tag::Link {
                    link_type: _,
                    dest_url,
                    title: _,
                    id: _,
                } => {
                    link_url.clear();
                    link_url.push_str(&dest_url);
                }
                _ => {}
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::Paragraph => {
                    let color = if in_blockquote {
                        Color::DarkGrey
                    } else {
                        Color::White
                    };
                    flush_acc(&acc, color, max_width, &mut result);
                    acc.clear();
                }
                TagEnd::Heading(_) => {
                    flush_acc(&acc, Color::Cyan, max_width, &mut result);
                    acc.clear();
                    in_heading = false;
                    result.push(LineEntry {
                        syntax_spans: None,
                        text: CompactString::new(""),
                        color: Color::White,
                        theme_accent: false,
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
                        result.push(LineEntry {
                            text: CompactString::from(trimmed),
                            color: Color::DarkYellow,
                            syntax_spans,
                            theme_accent: false,
                        });
                    }
                    if acc.is_empty() || acc.ends_with('\n') {
                        result.push(LineEntry {
                            text: CompactString::new(""),
                            color: Color::DarkYellow,
                            syntax_spans: None,
                            theme_accent: false,
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
                    });
                }
                TagEnd::Item => {
                    let color = if in_blockquote {
                        Color::DarkGrey
                    } else {
                        Color::White
                    };
                    prefixed.clear();
                    if ordered_list {
                        use std::fmt::Write as _;
                        let _ = write!(prefixed, " {list_item_count}. ");
                    } else {
                        prefixed.push_str(bullet_prefix(color));
                    }
                    let bullet_len = prefixed.len();
                    let mut item_lines = Vec::new();
                    let mut first = true;
                    for line in acc.split('\n') {
                        let trimmed = line.trim_end_matches('\r');
                        if trimmed.is_empty() {
                            item_lines.push(LineEntry {
                                syntax_spans: None,
                                text: CompactString::new(""),
                                color,
                                theme_accent: false,
                            });
                        } else if first {
                            prefixed.truncate(bullet_len);
                            prefixed.push_str(trimmed);
                            for chunk in word_wrap(&prefixed, max_width) {
                                item_lines.push(LineEntry {
                                    syntax_spans: None,
                                    text: chunk,
                                    color,
                                    theme_accent: false,
                                });
                            }
                            first = false;
                        } else {
                            for chunk in word_wrap(trimmed, max_width) {
                                item_lines.push(LineEntry {
                                    syntax_spans: None,
                                    text: chunk,
                                    color,
                                    theme_accent: false,
                                });
                            }
                        }
                    }
                    result.extend(item_lines);
                    acc.clear();
                }
                TagEnd::List(_) => {
                    ordered_list = false;
                    list_item_count = 0;
                    result.push(LineEntry {
                        syntax_spans: None,
                        text: CompactString::new(""),
                        color: Color::White,
                        theme_accent: false,
                    });
                }
                TagEnd::Link => {
                    if !link_url.is_empty() {
                        if in_table_cell {
                            table_cell.push_str(" (");
                            table_cell.push_str(&link_url);
                            table_cell.push(')');
                        } else if !acc.is_empty() {
                            flush_acc(&acc, Color::DarkCyan, max_width, &mut result);
                            prefixed.clear();
                            prefixed.push_str("  ↪ ");
                            prefixed.push_str(&link_url);
                            for chunk in word_wrap(&prefixed, max_width) {
                                result.push(LineEntry {
                                    syntax_spans: None,
                                    text: chunk,
                                    color: Color::DarkGrey,
                                    theme_accent: false,
                                });
                            }
                            acc.clear();
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
                    acc.push_str(&t);
                }
            }
            Event::Code(t) => {
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
                });
                result.push(LineEntry {
                    syntax_spans: None,
                    text: CompactString::new(""),
                    color: Color::White,
                    theme_accent: false,
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

    StyledMarkdown {
        lines: result,
        code_state: last_code_state,
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
}
