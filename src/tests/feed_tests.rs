use crate::ui::feed::{BlockStyle, Feed, FeedLines};
use crate::ui::renderer::LineEntry;
use crossterm::style::Color;
use std::sync::Arc;

// Traverse the lookup used by the viewport, including segment boundaries.
fn indexed_rows(lines: &FeedLines) -> impl Iterator<Item = &LineEntry> {
    (0..lines.len()).map(|index| &lines[index])
}

#[track_caller]
fn assert_same_rows(actual: &FeedLines, expected: &FeedLines) {
    assert_eq!(actual.len(), expected.len(), "visual row count");
    for index in 0..actual.len() {
        assert_eq!(actual[index].text, expected[index].text, "row {index} text");
        assert_eq!(
            actual[index].color, expected[index].color,
            "row {index} color"
        );
    }
}

#[test]
fn block_style_color_mapping() {
    assert_eq!(BlockStyle::User.color(), Color::Green);
    assert_eq!(BlockStyle::Agent.color(), Color::White);
    assert_eq!(BlockStyle::Reasoning.color(), Color::DarkMagenta);
    assert_eq!(BlockStyle::Tool.color(), Color::Yellow);
    assert_eq!(BlockStyle::ToolResult.color(), Color::DarkGrey);
    assert_eq!(BlockStyle::Error.color(), Color::Red);
    assert_eq!(BlockStyle::System.color(), Color::DarkGrey);
    assert_eq!(BlockStyle::Welcome.color(), Color::Cyan);
    assert_eq!(BlockStyle::Permission.color(), Color::Magenta);
    assert_eq!(BlockStyle::Plain.color(), Color::White);
}

#[test]
fn lines_wrap_plain_block() {
    let mut feed = Feed::new();
    feed.push_line(BlockStyle::Plain, "hello world");
    let lines = feed.lines(20);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].text, "hello world");
    assert_eq!(lines[0].color, Color::White);
}

#[test]
fn lines_wrap_narrow_width() {
    let mut feed = Feed::new();
    feed.push_line(BlockStyle::Plain, "hello world");
    let lines = feed.lines(5);
    assert!(lines.len() > 1);
    for line in indexed_rows(&lines) {
        assert!(line.text.chars().count() <= 5 || line.text == "hello" || line.text == "world");
    }
}

#[test]
fn empty_block_produces_empty_line() {
    let mut feed = Feed::new();
    feed.push_line(BlockStyle::Plain, "");
    let lines = feed.lines(80);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].text, "");
}

#[test]
fn agent_block_gets_prefix_and_markdown() {
    let mut feed = Feed::new();
    feed.push_block(BlockStyle::Agent, "hello **world**");
    let lines = feed.lines(80);
    assert!(lines.len() > 0);
    assert!(
        lines[0].text.starts_with("< "),
        "first agent line should start with '< ', got {:?}",
        lines[0].text
    );
    let joined: String = indexed_rows(&lines)
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("");
    assert!(
        joined.contains("hello "),
        "prose should be present: {}",
        joined
    );
    assert!(
        joined.contains("world"),
        "bold text should be present: {}",
        joined
    );
}

#[test]
fn agent_empty_block_no_lines() {
    let mut feed = Feed::new();
    feed.push_block(BlockStyle::Agent, "");
    let lines = feed.lines(80);
    assert_eq!(lines.len(), 0);
}

#[test]
fn line_count_matches_lines() {
    let mut feed = Feed::new();
    feed.push_line(BlockStyle::Plain, "one");
    feed.push_line(BlockStyle::Plain, "two");
    feed.push_line(BlockStyle::Plain, "three");
    assert_eq!(feed.line_count(80), 3);
}

#[test]
fn repeated_layout_hits_share_the_cached_line_vector() {
    let mut feed = Feed::new();
    feed.push_line(BlockStyle::Plain, "shared");

    let first = feed.lines(80);
    let second = feed.lines(80);

    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(feed.layout_computes(), 1);
}

#[test]
fn streaming_rerenders_only_the_active_block() {
    let mut feed = Feed::new();
    for index in 0..100 {
        feed.push_line(BlockStyle::Plain, format!("completed {index}"));
    }
    feed.push_streaming_block(BlockStyle::Agent);
    let _ = feed.lines(80);
    assert_eq!(feed.block_renders(), 101);

    for _ in 0..20 {
        assert!(feed.append_to_last("token "));
        let _ = feed.lines(80);
    }
    assert_eq!(
        feed.block_renders(),
        121,
        "completed blocks must stay in their per-block render caches"
    );
}

#[test]
fn streaming_layout_shares_completed_row_allocations() {
    let mut feed = Feed::new();
    feed.push_line(BlockStyle::Plain, "completed system row");
    feed.push_streaming_block(BlockStyle::Agent);
    assert!(feed.append_to_last("completed agent paragraph\n\n"));
    let before = feed.lines(80);
    let before_segments = before.segment_ptrs_for_test();

    assert!(feed.append_to_last("unfinished tail"));
    let after = feed.lines(80);
    let after_segments = after.segment_ptrs_for_test();

    assert_eq!(before_segments[0], after_segments[0]);
    assert_eq!(before_segments[1], after_segments[1]);
    assert!(after_segments.len() > before_segments.len());
}

#[test]
fn feed_retention_bounds_completed_blocks() {
    let mut feed = Feed::new();
    let (max_blocks, _, _) = Feed::retention_limits_for_test();
    for index in 0..max_blocks + 7 {
        feed.push_line(BlockStyle::Plain, format!("row {index}"));
    }

    assert_eq!(feed.block_count(), max_blocks);
    assert_eq!(feed.block_text(0), Some("row 7"));
    let newest = format!("row {}", max_blocks + 6);
    assert_eq!(feed.block_text(max_blocks - 1), Some(newest.as_str()));
    let lines = feed.lines(80);
    assert_eq!(lines[0].text, "[7 earlier feed blocks omitted]");
}

#[test]
fn feed_retention_bounds_bytes_and_compacts_one_large_stream() {
    let mut feed = Feed::new();
    let (_, max_feed_bytes, max_block_bytes) = Feed::retention_limits_for_test();
    let payload = "x".repeat(max_block_bytes / 2);
    for _ in 0..20 {
        feed.push_line(BlockStyle::Plain, &payload);
    }
    assert!(feed.total_bytes_for_test() <= max_feed_bytes);

    feed.clear();
    feed.push_streaming_block(BlockStyle::Agent);
    assert!(feed.append_to_last("é".repeat(max_block_bytes).as_str()));
    let retained = feed.block_text(0).expect("compacted stream");
    assert!(retained.starts_with("[earlier feed content omitted]"));
    assert!(retained.len() < max_block_bytes);
    assert!(retained.is_char_boundary(retained.len()));
}

#[test]
fn feed_retention_preserves_live_block_index_while_shedding_foreign_blocks() {
    let mut feed = Feed::new();
    let (max_blocks, _, _) = Feed::retention_limits_for_test();
    for index in 0..max_blocks - 1 {
        feed.push_line(BlockStyle::Plain, format!("history {index}"));
    }
    feed.push_streaming_block(BlockStyle::Agent);
    let live = feed.block_count() - 1;
    assert!(feed.append_to(live, "first"));

    for index in 0..10 {
        feed.push_line(BlockStyle::System, format!("foreign {index}"));
        assert!(feed.append_to(live, " token"));
    }

    assert!(feed.is_streaming(live));
    assert!(feed.block_count() <= max_blocks);
    assert!(
        feed.block_text(live)
            .is_some_and(|text| text.ends_with(" token"))
    );
}

#[test]
fn visible_range_bottom_aligned_when_short() {
    let mut feed = Feed::new();
    feed.push_line(BlockStyle::Plain, "one");
    feed.push_line(BlockStyle::Plain, "two");
    let (start, end) = feed.visible_range(80, 0, 10);
    assert_eq!(start, 0);
    assert_eq!(end, 2);
}

#[test]
fn visible_range_scrolled() {
    let mut feed = Feed::new();
    for i in 0..20 {
        feed.push_line(BlockStyle::Plain, format!("line {}", i));
    }
    let (start, end) = feed.visible_range(80, 5, 10);
    assert_eq!(end - start, 10);
    assert_eq!(start, 5);
}

#[test]
fn line_at_visual_row_bottom_pad() {
    let mut feed = Feed::new();
    feed.push_line(BlockStyle::Plain, "one");
    // viewport height 10, auto-scroll, content shorter than viewport -> padding
    assert_eq!(feed.line_at_visual_row(80, 0, 10, 0), None);
    assert_eq!(feed.line_at_visual_row(80, 0, 10, 9), Some(0));
}

#[test]
fn line_at_visual_row_scrolled() {
    let mut feed = Feed::new();
    for i in 0..20 {
        feed.push_line(BlockStyle::Plain, format!("line {}", i));
    }
    assert_eq!(feed.line_at_visual_row(80, 5, 10, 0), Some(5));
    assert_eq!(feed.line_at_visual_row(80, 5, 10, 9), Some(14));
}

#[test]
fn selected_text_extracts_lines() {
    let mut feed = Feed::new();
    feed.push_line(BlockStyle::Plain, "alpha");
    feed.push_line(BlockStyle::Plain, "beta");
    feed.push_line(BlockStyle::Plain, "gamma");
    let text = feed.selected_text(80, 0, 2);
    assert_eq!(text.as_deref(), Some("alpha\nbeta\ngamma"));
}

#[test]
fn selected_text_reversed_range() {
    let mut feed = Feed::new();
    feed.push_line(BlockStyle::Plain, "alpha");
    feed.push_line(BlockStyle::Plain, "beta");
    let text = feed.selected_text(80, 1, 0);
    assert_eq!(text.as_deref(), Some("alpha\nbeta"));
}

#[test]
fn append_to_last_extends_block() {
    let mut feed = Feed::new();
    feed.push_block(BlockStyle::Agent, "hello");
    assert!(feed.append_to_last(" world"));
    let lines = feed.lines(80);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].text.contains("hello world"));
}

#[test]
fn append_to_last_returns_false_when_empty() {
    let mut feed = Feed::new();
    assert!(!feed.append_to_last("orphan"));
}

#[test]
fn replace_last_updates_final_block() {
    let mut feed = Feed::new();
    feed.push_line(BlockStyle::Plain, "first");
    feed.push_line(BlockStyle::Plain, "second");
    feed.replace_last(BlockStyle::Agent, "replaced");
    let lines = feed.lines(80);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].text, "first");
    assert_eq!(lines[1].text, "< replaced");
}

#[test]
fn replace_last_pushes_when_empty() {
    let mut feed = Feed::new();
    feed.replace_last(BlockStyle::Agent, "only");
    let lines = feed.lines(80);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].text, "< only");
}

#[test]
fn truncate_blocks_keeps_prefix() {
    let mut feed = Feed::new();
    feed.push_line(BlockStyle::Plain, "first");
    feed.push_line(BlockStyle::Plain, "second");
    feed.push_line(BlockStyle::Plain, "third");
    feed.truncate_blocks(2);
    assert_eq!(feed.block_count(), 2);
    let lines = feed.lines(80);
    assert_eq!(lines.len(), 2);
}

#[test]
fn clear_empties_feed() {
    let mut feed = Feed::new();
    feed.push_line(BlockStyle::Plain, "hello");
    feed.clear();
    assert!(feed.is_empty());
    assert_eq!(feed.line_count(80), 0);
}

#[test]
fn generation_starts_at_zero() {
    let feed = Feed::new();
    assert_eq!(feed.generation(), 0);
}

#[test]
fn generation_bumps_on_each_mutator() {
    let mut feed = Feed::new();
    feed.push_block(BlockStyle::Plain, "one");
    assert_eq!(feed.generation(), 1);
    feed.push_line(BlockStyle::Plain, "two");
    assert_eq!(feed.generation(), 2);
    assert!(feed.append_to_last(" more"));
    assert_eq!(feed.generation(), 3);
    feed.replace_last(BlockStyle::Agent, "replaced");
    assert_eq!(feed.generation(), 4);
    feed.truncate_blocks(1);
    assert_eq!(feed.generation(), 5);
    feed.clear();
    assert_eq!(feed.generation(), 6);
}

#[test]
fn generation_not_bumped_by_failed_append() {
    let mut feed = Feed::new();
    assert!(!feed.append_to_last("orphan"));
    assert_eq!(feed.generation(), 0);
}

#[test]
fn generation_not_bumped_by_reads() {
    let mut feed = Feed::new();
    feed.push_line(BlockStyle::Plain, "one");
    let before = feed.generation();
    let _ = feed.lines(80);
    let _ = feed.line_count(80);
    let _ = feed.visible_range(80, 0, 10);
    let _ = feed.line_at_visual_row(80, 0, 10, 0);
    let _ = feed.selected_text(80, 0, 0);
    let _ = feed.is_empty();
    let _ = feed.block_count();
    assert_eq!(feed.generation(), before);
}

#[test]
fn running_agent_block_renders_tail_as_plain_text() {
    let mut feed = Feed::new();
    feed.push_streaming_block(BlockStyle::Agent);
    assert!(feed.append_to_last("hello **wor"));
    let lines = feed.lines(80);
    assert_eq!(lines.len(), 1);
    // No markdown parsing while the line is unfinished: markers stay literal.
    assert_eq!(lines[0].text, "< hello **wor");
    assert_eq!(lines[0].color, Color::White);
}

#[test]
fn running_agent_block_parses_only_completed_lines() {
    let mut feed = Feed::new();
    feed.push_streaming_block(BlockStyle::Agent);
    assert!(feed.append_to_last("first **bold**\nsecond **par"));
    let lines = feed.lines(80);
    assert_eq!(lines.len(), 2);
    // The completed line is parsed as markdown: bold markers are gone.
    assert_eq!(lines[0].text, "< first bold");
    // The unfinished tail line stays plain: markers remain literal.
    assert_eq!(lines[1].text, "second **par");
}

#[test]
fn running_agent_block_appends_grow_tail() {
    let mut feed = Feed::new();
    feed.push_streaming_block(BlockStyle::Agent);
    assert!(feed.append_to_last("hello"));
    assert!(feed.append_to_last(" world"));
    let lines = feed.lines(80);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].text, "< hello world");
}

#[test]
fn finalize_last_parses_full_text() {
    let mut feed = Feed::new();
    feed.push_streaming_block(BlockStyle::Agent);
    assert!(feed.append_to_last("hello **world**"));
    feed.finalize_last();
    let lines = feed.lines(80);
    assert_eq!(lines.len(), 1);
    // After finalizing, the former tail line is parsed as markdown.
    assert_eq!(lines[0].text, "< hello world");
}

#[test]
fn finalize_last_bumps_generation_once() {
    let mut feed = Feed::new();
    feed.push_streaming_block(BlockStyle::Agent);
    let before = feed.generation();
    feed.finalize_last();
    assert_eq!(feed.generation(), before + 1);
    // Second call is a no-op: the block is no longer running.
    feed.finalize_last();
    assert_eq!(feed.generation(), before + 1);
}

#[test]
fn finalize_last_on_complete_block_is_noop() {
    let mut feed = Feed::new();
    feed.push_block(BlockStyle::Agent, "done");
    let before = feed.generation();
    feed.finalize_last();
    assert_eq!(feed.generation(), before);
}

#[test]
fn replace_last_invalidates_cached_layout() {
    let mut feed = Feed::new();
    feed.push_block(BlockStyle::Agent, "aaaa **old**");
    let _ = feed.lines(80); // populate the layout cache
    // Same length, different content: the cached layout must not leak through.
    feed.replace_last(BlockStyle::Agent, "bbbb **new**");
    let lines = feed.lines(80);
    let joined: String = indexed_rows(&lines)
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("");
    assert!(joined.contains("new"), "expected new content: {joined}");
    assert!(!joined.contains("old"), "stale cached content: {joined}");
}

#[test]
fn agent_layout_recomputes_on_width_change() {
    let mut feed = Feed::new();
    feed.push_block(
        BlockStyle::Agent,
        "one two three four five six seven eight nine ten eleven twelve",
    );
    let wide = feed.lines(120);
    let narrow = feed.lines(20);
    assert!(
        narrow.len() > wide.len(),
        "narrow width should wrap into more lines: {} vs {}",
        narrow.len(),
        wide.len()
    );
}

#[test]
fn scroll_and_selection_queries_reuse_prewrapped_rows() {
    let mut feed = Feed::new();
    feed.push_line(BlockStyle::Plain, "hello");
    let _ = feed.lines(80);
    let _ = feed.line_count(80);
    let _ = feed.visible_range(80, 0, 10);
    let _ = feed.selected_text(80, 0, 0);
    let _ = feed.line_at_visual_row(80, 0, 10, 9);
    assert_eq!(
        feed.layout_computes(),
        1,
        "scroll/selection queries should reuse the pre-wrapped rows"
    );

    feed.push_line(BlockStyle::Plain, "world");
    let _ = feed.lines(80);
    assert_eq!(feed.layout_computes(), 2, "mutation should invalidate");

    let _ = feed.lines(40);
    assert_eq!(feed.layout_computes(), 3, "resize should invalidate");

    // Alternating back to a previously seen width still re-lays out once
    // (single-slot cache), then reuses.
    let _ = feed.lines(80);
    let _ = feed.lines(80);
    assert_eq!(feed.layout_computes(), 4);
}

#[test]
fn streaming_within_stable_boundary_is_subquadratic() {
    // Test that appending lines within a stable boundary (after a blank line)
    // achieves sub-quadratic parsing: O(n) not O(n^2).
    //
    // Without the optimization, each agent_block_lines call would re-parse the
    // entire text[0..completed_len], giving O(n^2) total bytes parsed.
    // With the optimization, stable lines are reused, giving O(n) work.
    //
    // We measure total bytes sent to markdown_to_styled and verify it's bounded
    // by roughly 2-3x the final text length (one parse for the stable part, one
    // for the extended part, plus some overhead).
    let mut feed = Feed::new();
    feed.push_streaming_block(BlockStyle::Agent);

    // Append lines with proper blank-line boundaries to enable the optimization.
    // Each "paragraph" is 2 lines, separated by a blank line.
    let mut total_text_len = 0;
    for para in 0..20 {
        let text = format!("line {}_a\nline {}_b\n\n", para, para);
        total_text_len += text.len();
        assert!(feed.append_to_last(&text));
        let _ = feed.lines(80);
    }

    let bytes_parsed = feed.markdown_bytes_parsed();

    // With the optimization, we expect bytes_parsed to be roughly 2-3x the final text.
    // Without optimization (naive full re-parse each time), we'd see O(n^2):
    // - ~40 appends, each re-parsing the entire prefix: sum of 1+2+3+...+40 ~ 820x bytes
    // - Much larger than what we'll observe with the optimization.
    assert!(
        bytes_parsed <= total_text_len * 3,
        "streaming with stable boundaries should be sub-quadratic; \
         total_text_len={}, bytes_parsed={}, ratio={:.2}x",
        total_text_len,
        bytes_parsed,
        bytes_parsed as f64 / total_text_len as f64
    );
}

#[test]
fn streaming_correctness_with_stable_boundary_enabled() {
    // Verify that with proper stable-boundary detection, the output equals
    // a from-scratch parse. This is the correctness check that the boundary
    // detection doesn't break rendering.
    let mut feed = Feed::new();
    feed.push_streaming_block(BlockStyle::Agent);

    // Append lines up to a stable boundary (blank line).
    assert!(feed.append_to_last("line one\nline two\n\n"));
    let _ = feed.lines(80);

    // Append more lines after the stable boundary.
    for i in 3..10 {
        assert!(feed.append_to_last(format!("line {}\n", i)));
        let _ = feed.lines(80);
    }

    // Get the incremental parse result.
    let incremental_lines = feed.lines(80);

    // Create a fresh feed and parse the full text at once for comparison.
    let mut fresh_feed = Feed::new();
    let full_text =
        "line one\nline two\n\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\n";
    fresh_feed.push_block(BlockStyle::Agent, full_text);
    let fresh_lines = fresh_feed.lines(80);

    assert_same_rows(&incremental_lines, &fresh_lines);
}

#[test]
fn streaming_reference_definition_reparses_earlier_references() {
    let mut feed = Feed::new();
    feed.push_streaming_block(BlockStyle::Agent);
    assert!(feed.append_to_last("Read the [guide] for details.\n\n"));
    let _ = feed.lines(80);
    assert!(feed.append_to_last("[guide]: https://example.com/guide\n"));

    let incremental = feed.lines(80);
    let mut fresh = Feed::new();
    fresh.push_block(
        BlockStyle::Agent,
        "Read the [guide] for details.\n\n[guide]: https://example.com/guide\n",
    );
    let reparsed = fresh.lines(80);

    assert_same_rows(&incremental, &reparsed);
}

#[test]
fn streaming_indented_code_continues_across_blank_line() {
    let mut feed = Feed::new();
    feed.push_streaming_block(BlockStyle::Agent);
    assert!(feed.append_to_last("Intro.\n\n    alpha\n\n"));
    let _ = feed.lines(80);
    assert!(feed.append_to_last("    beta\n"));

    let incremental = feed.lines(80);
    let mut fresh = Feed::new();
    fresh.push_block(BlockStyle::Agent, "Intro.\n\n    alpha\n\n    beta\n");
    let reparsed = fresh.lines(80);

    assert_same_rows(&incremental, &reparsed);
}

#[test]
fn streaming_loose_list_produces_correct_output() {
    // Test correctness: a loose list (with blank lines between items) keeps
    // the list open across blank lines. The blank line between items is not
    // a top-level block boundary.
    let mut feed = Feed::new();
    feed.push_streaming_block(BlockStyle::Agent);

    // Start a loose list with blank line between items.
    assert!(feed.append_to_last("- item one\n\n"));
    let _ = feed.lines(80);
    assert!(feed.append_to_last("- item two\n"));

    let incremental_lines = feed.lines(80);

    // Parse from scratch for comparison.
    feed.finalize_last();
    let fresh_parse_lines = feed.lines(80);

    assert_same_rows(&incremental_lines, &fresh_parse_lines);
}

#[test]
fn streaming_setext_heading_produces_correct_output() {
    // Test correctness: setext-style headings are retroactive (the underline
    // makes the previous line a heading). This is a key edge case for
    // incremental parsing.
    let mut feed = Feed::new();
    feed.push_streaming_block(BlockStyle::Agent);

    // Add a line that will become a setext heading when the underline arrives.
    assert!(feed.append_to_last("This is a heading\n"));
    let intermediate_lines = feed.lines(80);

    // Add the underline that retroactively makes it a heading.
    assert!(feed.append_to_last("==================\n"));
    let final_lines = feed.lines(80);

    // Parse from scratch.
    feed.finalize_last();
    let fresh_parse_lines = feed.lines(80);

    // The text should be the same between the two approaches.
    let intermediate_text: String = indexed_rows(&intermediate_lines)
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let fresh_text: String = indexed_rows(&fresh_parse_lines)
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    // Before the underline, the line should be treated as plain text.
    assert!(
        intermediate_text.contains("This is a heading"),
        "intermediate should contain the heading text"
    );

    // After the underline, it becomes a setext heading in the final parse.
    // The final text after adding the underline should match the fresh parse.
    assert_same_rows(&final_lines, &fresh_parse_lines);

    // The final result should also contain the heading text.
    assert!(fresh_text.contains("This is a heading"));
}

#[test]
fn finalized_streaming_block_equals_from_scratch_parse() {
    // General correctness test: any streaming sequence should produce the
    // same output as parsing the final text from scratch.
    let mut feed = Feed::new();
    feed.push_streaming_block(BlockStyle::Agent);

    let chunks = vec![
        "# Heading\n",
        "\n",
        "Some **bold** text.\n",
        "\n",
        "```\n",
        "code block\n",
        "```\n",
        "\n",
        "- List item 1\n",
        "- List item 2\n",
    ];

    for chunk in &chunks {
        assert!(feed.append_to_last(chunk));
        let _ = feed.lines(80);
    }

    // Get the incremental parse result.
    let incremental_lines = feed.lines(80);

    // Create a fresh feed and parse the entire text at once.
    let full_text: String = chunks.join("");
    let mut fresh_feed = Feed::new();
    fresh_feed.push_block(BlockStyle::Agent, &full_text);
    let fresh_lines = fresh_feed.lines(80);

    assert_same_rows(&incremental_lines, &fresh_lines);
}

// --- indexed streaming: foreign blocks pushed mid-stream must survive ---

#[test]
fn append_to_targets_tracked_block_not_last() {
    let mut feed = Feed::new();
    feed.push_streaming_block(BlockStyle::Agent);
    let idx = feed.block_count() - 1;
    assert!(feed.append_to(idx, "hello"));
    // A `/btw` answer lands after the streaming block mid-stream.
    feed.push_line(BlockStyle::System, "[btw #1] answer");
    assert!(feed.append_to(idx, " world"));
    assert_eq!(feed.block_text(idx), Some("hello world"));
    assert_eq!(feed.block_text(idx + 1), Some("[btw #1] answer"));
}

#[test]
fn append_to_returns_false_for_missing_block() {
    let mut feed = Feed::new();
    assert!(!feed.append_to(0, "orphan"));
    feed.push_block(BlockStyle::Agent, "x");
    assert!(!feed.append_to(5, "orphan"));
    assert!(feed.append_to(0, "y"));
    assert_eq!(feed.block_text(0), Some("xy"));
}

#[test]
fn streaming_block_finalized_in_place_keeps_foreign_blocks_and_tokens() {
    let mut feed = Feed::new();
    feed.push_streaming_block(BlockStyle::Agent);
    let idx = feed.block_count() - 1;
    assert!(feed.is_streaming(idx));
    assert!(feed.append_to(idx, "first **bold**\n"));
    feed.push_line(BlockStyle::System, "queued: next question");
    assert!(feed.append_to(idx, "second **par**"));

    let before = feed.generation();
    feed.finalize_block(idx);
    assert!(!feed.is_streaming(idx));
    assert!(feed.generation() > before);
    // Nothing truncated, nothing glued to the foreign block.
    assert_eq!(feed.block_count(), 2);
    assert_eq!(feed.block_text(idx), Some("first **bold**\nsecond **par**"));
    assert_eq!(feed.block_text(idx + 1), Some("queued: next question"));

    // Finalized text is parsed as markdown (bold markers consumed), and the
    // foreign block still renders after it.
    let lines = feed.lines(80);
    let joined: Vec<&str> = indexed_rows(&lines).map(|l| l.text.as_str()).collect();
    assert!(
        joined.iter().any(|t| t.contains("first bold")),
        "{joined:?}"
    );
    assert!(
        joined.iter().any(|t| t.contains("second par")),
        "{joined:?}"
    );
    assert!(
        joined.iter().any(|t| t.contains("queued: next question")),
        "{joined:?}"
    );
    let agent_pos = joined
        .iter()
        .position(|t| t.contains("second par"))
        .unwrap();
    let foreign_pos = joined.iter().position(|t| t.contains("queued:")).unwrap();
    assert!(agent_pos < foreign_pos);
}

#[test]
fn finalize_block_is_noop_out_of_range_or_not_running() {
    let mut feed = Feed::new();
    feed.push_block(BlockStyle::Agent, "done");
    let before = feed.generation();
    feed.finalize_block(0);
    feed.finalize_block(7);
    assert_eq!(feed.generation(), before);
    assert!(!feed.is_streaming(0));
    assert!(!feed.is_streaming(7));
}

// ── Fenced streaming: linear work and exact delimiter rules ────────────

/// Stream `lines` one at a time into a running agent block, laying out after
/// each, and return the total bytes handed to the markdown parser.
fn stream_lines_and_measure(lines: &[String]) -> (usize, usize) {
    let mut feed = Feed::new();
    feed.push_streaming_block(BlockStyle::Agent);
    let mut source_bytes = 0;
    for line in lines {
        source_bytes += line.len();
        assert!(feed.append_to_last(line));
        let _ = feed.lines(80);
    }
    (source_bytes, feed.markdown_bytes_parsed())
}

fn fenced_source(line_count: usize) -> Vec<String> {
    let mut lines = vec!["```rust\n".to_string()];
    for index in 0..line_count {
        lines.push(format!("    let value_{index:04} = compute({index:04});\n"));
    }
    lines
}

#[test]
fn streaming_a_long_fence_parses_a_bounded_multiple_of_its_source() {
    let lines = fenced_source(1_000);
    let (source_bytes, parsed_bytes) = stream_lines_and_measure(&lines);
    assert!(
        parsed_bytes <= source_bytes * 4,
        "a fenced response must not re-parse the fence per line: \
         source={source_bytes}, parsed={parsed_bytes}, ratio={:.1}x",
        parsed_bytes as f64 / source_bytes as f64
    );
}

#[test]
fn streaming_fence_parsing_work_grows_near_linearly() {
    let mut ratios = Vec::new();
    for count in [1_000usize, 2_000, 4_000] {
        let lines = fenced_source(count);
        let (source_bytes, parsed_bytes) = stream_lines_and_measure(&lines);
        ratios.push(parsed_bytes as f64 / source_bytes as f64);
    }
    // Quadratic work would multiply the ratio with every doubling.
    assert!(
        ratios[2] <= ratios[0] * 1.5,
        "fence parsing work is not near-linear: {ratios:?}"
    );
}

/// Parsed bytes per source byte for a construct that publishes no markdown
/// boundary of its own, at 1k/2k/4k lines. Bounded per-chunk work makes this
/// ratio flat; quadratic work multiplies it with every doubling.
fn unbroken_construct_ratios(line: impl Fn(usize) -> String) -> Vec<f64> {
    [1_000usize, 2_000, 4_000]
        .into_iter()
        .map(|count| {
            let lines: Vec<String> = (0..count).map(&line).collect();
            let (source_bytes, parsed_bytes) = stream_lines_and_measure(&lines);
            parsed_bytes as f64 / source_bytes as f64
        })
        .collect()
}

#[test]
fn streaming_a_long_list_does_not_grow_quadratically() {
    let ratios =
        unbroken_construct_ratios(|index| format!("- item {index:04} with descriptive text\n"));
    assert!(
        ratios[2] <= ratios[0] * 1.5,
        "list streaming work is not near-linear: {ratios:?}"
    );
}

#[test]
fn streaming_a_long_paragraph_does_not_grow_quadratically() {
    let ratios = unbroken_construct_ratios(|index| {
        format!("sentence {index:04} continuing the same paragraph\n")
    });
    assert!(
        ratios[2] <= ratios[0] * 1.5,
        "paragraph streaming work is not near-linear: {ratios:?}"
    );
}

/// Stream `text` one line at a time and compare the result with a from-scratch
/// parse of the same text.
#[track_caller]
fn assert_streamed_matches_fresh(text: &str) -> Arc<FeedLines> {
    let mut feed = Feed::new();
    feed.push_streaming_block(BlockStyle::Agent);
    for line in text.split_inclusive('\n') {
        assert!(feed.append_to_last(line));
        let _ = feed.lines(80);
    }
    let streamed = feed.lines(80);

    let mut fresh = Feed::new();
    fresh.push_block(BlockStyle::Agent, text);
    let reparsed = fresh.lines(80);

    assert_same_rows(&streamed, &reparsed);
    streamed
}

#[test]
fn a_shorter_delimiter_inside_a_longer_fence_stays_code() {
    // A four-backtick fence is not closed by an embedded three-backtick line,
    // so the literal '#' after it must never be streamed as a heading.
    assert_streamed_matches_fresh("````\ncode\n```\n\n# literal header\n````\n\nafter\n");
}

#[test]
fn fence_delimiter_lengths_and_characters_round_trip() {
    for delimiter in ["```", "````", "`````", "~~~", "~~~~"] {
        let text = format!("{delimiter}\ninner\n\n# not a heading\n{delimiter}\n\ndone\n");
        assert_streamed_matches_fresh(&text);
    }
}

#[test]
fn a_tilde_fence_is_not_closed_by_backticks() {
    assert_streamed_matches_fresh("~~~\ncode\n```\n\n# literal\n~~~\n\nafter\n");
}

#[test]
fn a_closing_delimiter_with_trailing_text_does_not_close_the_fence() {
    assert_streamed_matches_fresh("```\ncode\n``` not a close\n\n# literal\n```\n\nafter\n");
}

#[test]
fn an_indented_fence_round_trips() {
    assert_streamed_matches_fresh("  ```rust\n  fn main() {}\n\n# literal\n  ```\n\nafter\n");
}

#[test]
fn an_info_string_with_a_backtick_does_not_open_a_fence() {
    assert_streamed_matches_fresh("``` not`valid\n\n# heading\n\nafter\n");
}

#[test]
fn a_fence_inside_a_list_round_trips() {
    assert_streamed_matches_fresh("- item\n\n  ```\n  code\n  ```\n\n- next\n");
}

#[test]
fn an_unterminated_fence_round_trips_while_streaming() {
    assert_streamed_matches_fresh("intro\n\n```rust\nfn a() {}\nfn b() {}\n");
}

#[test]
fn a_blank_line_inside_a_fence_round_trips() {
    let lines = assert_streamed_matches_fresh("```\nalpha\n\nbeta\n```\n\nafter\n");
    // Both layouts share the boundary detector, so also pin the semantics:
    // a blank keeps the fence open, while the closing delimiter restores prose.
    let beta = indexed_rows(&lines)
        .find(|line| line.text == "beta")
        .unwrap();
    assert_eq!(beta.color, Color::DarkYellow);
    let after = indexed_rows(&lines)
        .find(|line| line.text == "after")
        .unwrap();
    assert_eq!(after.color, Color::White);
}
