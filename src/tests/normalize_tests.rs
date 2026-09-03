use crate::agent::tools::normalize::{levenshtein_similarity, normalize_whitespace};

#[test]
fn normalize_tabs_to_spaces() {
    assert_eq!(
        normalize_whitespace("\tfn foo() {\n\t    bar\n\t}\n"),
        "    fn foo() {\n        bar\n    }\n"
    );
}

#[test]
fn normalize_trailing_spaces() {
    assert_eq!(normalize_whitespace("hello   \nworld\n"), "hello\nworld\n");
}

#[test]
fn normalize_collapse_blank_lines() {
    assert_eq!(normalize_whitespace("a\n\n\nb\n"), "a\n\nb\n");
}

#[test]
fn levenshtein_identical() {
    assert!((levenshtein_similarity("hello", "hello") - 1.0).abs() < 0.001);
}

#[test]
fn levenshtein_similar() {
    let sim = levenshtein_similarity("hello world", "helo world");
    assert!(sim > 0.85, "expected >0.85, got {sim}");
}

#[test]
fn levenshtein_different() {
    let sim = levenshtein_similarity("hello", "zzzzz");
    assert!(sim < 0.4, "expected <0.4, got {sim}");
}

// ── Normalized-to-source byte mapping ──────────────────────────────────

use crate::agent::tools::normalize::NormalizedText;

#[test]
fn mapped_text_matches_plain_normalizer() {
    for input in [
        "",
        "abc",
        "abc\n",
        "\tfn foo() {\n\t    bar\n\t}\n",
        "hello   \nworld\n",
        "a\n\n\nb\n",
        "a\r\n\r\n\r\nb\r\n",
        "x  \t \n\n\n\n   y\t\n\n",
    ] {
        assert_eq!(NormalizedText::new(input).text, normalize_whitespace(input));
    }
}

#[test]
fn mapped_range_skips_trailing_whitespace_before_match() {
    // "foo   \n    bar\n": the trailing spaces on line 1 vanish from the
    // normalized text, so a match on line 2 must not be shifted backwards.
    let src = "foo   \n    bar\n";
    let norm = NormalizedText::new(src);
    let search = normalize_whitespace("\tbar");
    let pos = norm.text.find(&search).unwrap();
    let (start, end) = norm.source_range(pos, search.len());
    assert_eq!(&src[start..end], "    bar");
}

#[test]
fn mapped_range_skips_collapsed_blank_lines_before_match() {
    let src = "foo\n\n\n\n    bar\nbaz\n";
    let norm = NormalizedText::new(src);
    let search = normalize_whitespace("\tbar");
    let pos = norm.text.find(&search).unwrap();
    let (start, end) = norm.source_range(pos, search.len());
    assert_eq!(&src[start..end], "    bar");
}

#[test]
fn mapped_range_covers_tabs_inside_match() {
    let src = "fn a() {\n\tx = 1;\n\ty = 2;\n}\n";
    let norm = NormalizedText::new(src);
    let search = normalize_whitespace("    x = 1;\n    y = 2;");
    let pos = norm.text.find(&search).unwrap();
    let (start, end) = norm.source_range(pos, search.len());
    assert_eq!(&src[start..end], "\tx = 1;\n\ty = 2;");
}

#[test]
fn mapped_range_at_end_of_file_without_newline() {
    let src = "keep   \n\tlast";
    let norm = NormalizedText::new(src);
    let search = normalize_whitespace("    last");
    let pos = norm.text.find(&search).unwrap();
    let (start, end) = norm.source_range(pos, search.len());
    assert_eq!(&src[start..end], "\tlast");
    assert_eq!(end, src.len());
}

#[test]
fn mapped_range_absorbs_collapsed_blank_lines_inside_match() {
    let src = "a\n\n\n\nb\nc\n";
    let norm = NormalizedText::new(src);
    let search = normalize_whitespace("a\n\nb");
    let pos = norm.text.find(&search).unwrap();
    let (start, end) = norm.source_range(pos, search.len());
    assert_eq!(&src[start..end], "a\n\n\n\nb");
}

#[test]
fn mapped_range_handles_multibyte_chars() {
    let src = "héllo   \n\twörld\n";
    let norm = NormalizedText::new(src);
    let search = normalize_whitespace("    wörld");
    let pos = norm.text.find(&search).unwrap();
    let (start, end) = norm.source_range(pos, search.len());
    assert_eq!(&src[start..end], "\twörld");
}
