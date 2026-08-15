use crate::ui::feed::{BlockStyle, Feed};
use crossterm::style::Color;

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
    for line in &lines {
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
    assert!(!lines.is_empty());
    assert!(
        lines[0].text.starts_with("< "),
        "first agent line should start with '< ', got {:?}",
        lines[0].text
    );
    let joined: String = lines
        .iter()
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
    assert!(lines.is_empty());
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
    let joined: String = lines
        .iter()
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
        assert!(feed.append_to_last(&format!("line {}\n", i)));
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

    // The incremental and fresh parses should produce identical output.
    assert_eq!(
        incremental_lines.len(),
        fresh_lines.len(),
        "incremental vs fresh: line count should match"
    );

    for (i, (inc, fresh)) in incremental_lines.iter().zip(fresh_lines.iter()).enumerate() {
        assert_eq!(inc.text, fresh.text, "line {} text should match", i);
        assert_eq!(inc.color, fresh.color, "line {} color should match", i);
    }
}

#[test]
fn streaming_fence_with_blank_line_produces_correct_output() {
    // Test correctness: a fence containing a blank line, streamed line by line.
    // This is the critical test that would fail if find_stable_boundary used
    // naive "\n\n" search, since the blank line inside the fence is not a
    // top-level block boundary.
    let mut feed = Feed::new();
    feed.push_streaming_block(BlockStyle::Agent);

    // Append intro, then open a fence.
    assert!(feed.append_to_last("Here is code:\n\n"));
    assert!(feed.append_to_last("```rust\n"));
    assert!(feed.append_to_last("fn a() {}\n"));
    // Blank line inside the fence: not a top-level boundary.
    assert!(feed.append_to_last("\n"));
    assert!(feed.append_to_last("fn b() {}\n"));
    assert!(feed.append_to_last("```\n"));

    // Get the incremental parse result.
    let incremental_lines = feed.lines(80);

    // Parse from scratch for comparison.
    feed.finalize_last();
    let fresh_lines = feed.lines(80);

    // The incremental and fresh parses must produce identical output.
    assert_eq!(
        incremental_lines.len(),
        fresh_lines.len(),
        "fence with blank line: line count mismatch"
    );

    let incremental_text: String = incremental_lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let fresh_text: String = fresh_lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(
        incremental_text, fresh_text,
        "fence with blank line: full content mismatch"
    );
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

    // Verify they match.
    let incremental_text: String = incremental_lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let fresh_text: String = fresh_parse_lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(
        incremental_text, fresh_text,
        "loose list: incremental vs fresh content should match"
    );
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
    let intermediate_text: String = intermediate_lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let final_text: String = final_lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let fresh_text: String = fresh_parse_lines
        .iter()
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
    assert_eq!(
        final_text, fresh_text,
        "setext heading: incremental vs fresh result"
    );

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

    // They should match.
    assert_eq!(
        incremental_lines.len(),
        fresh_lines.len(),
        "incremental vs fresh: line count mismatch"
    );

    for (i, (inc, fresh)) in incremental_lines.iter().zip(fresh_lines.iter()).enumerate() {
        assert_eq!(inc.text, fresh.text, "line {} text mismatch", i);
        assert_eq!(inc.color, fresh.color, "line {} color mismatch", i);
    }
}
