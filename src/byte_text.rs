//! Shared byte-size text helpers: UTF-8-safe byte clipping and IEC size labels.
//!
//! Every function here has one contract so callers can pick by what they need:
//!
//! - [`clip_to_bytes`] cuts silently to a hard byte ceiling and reports whether
//!   it cut. Use it where the caller shows its own "truncated" signal.
//! - [`clip_with_marker`] cuts to a hard byte ceiling *including* a marker, so
//!   the user can see text was cut and the result never exceeds `max`.
//! - [`format_bytes`] renders a byte count in binary units (`KiB`, `MiB`, ...),
//!   the same spelling every limit message in the crate uses.
//!
//! `extras::truncate::truncate_cjk` keeps its older "marker is appended after
//! `max` bytes" contract; use [`clip_with_marker`] when the total must fit.

#[cfg(any(test, feature = "subagents", feature = "mcp"))]
use std::borrow::Cow;

/// The longest prefix of `s` that is at most `max` bytes and ends on a UTF-8
/// character boundary, with whether anything was cut.
pub(crate) fn clip_to_bytes(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    (&s[..s.floor_char_boundary(max)], true)
}

/// `s` unchanged when it fits in `max` bytes; otherwise a prefix followed by
/// `marker`, with the total guaranteed to be at most `max` bytes. When `max`
/// cannot even hold the marker the result is the marker clipped to `max` bytes.
#[cfg(any(test, feature = "subagents", feature = "mcp"))]
pub(crate) fn clip_with_marker<'a>(s: &'a str, max: usize, marker: &str) -> Cow<'a, str> {
    if s.len() <= max {
        return Cow::Borrowed(s);
    }
    if max < marker.len() {
        return Cow::Owned(clip_to_bytes(marker, max).0.to_owned());
    }
    let head = clip_to_bytes(s, max - marker.len()).0;
    let mut out = String::with_capacity(head.len() + marker.len());
    out.push_str(head);
    out.push_str(marker);
    Cow::Owned(out)
}

/// In-place [`clip_to_bytes`]; returns whether the text was cut.
#[cfg(any(test, feature = "mcp", all(feature = "js", feature = "skills")))]
pub(crate) fn truncate_to_bytes(text: &mut String, max: usize) -> bool {
    let (kept, cut) = clip_to_bytes(text, max);
    if cut {
        let end = kept.len();
        text.truncate(end);
    }
    cut
}

/// Binary-unit size label: whole `B` below 1 KiB, otherwise one decimal in
/// `KiB`, `MiB` or `GiB`. A value that rounds up to the next unit is promoted
/// (1048575 is `1.0 MiB`, never `1024.0 KiB`).
pub(crate) fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64;
    let mut unit = 0;
    while unit < UNITS.len() - 1 && (size >= 1024.0 || (size * 10.0).round() / 10.0 >= 1024.0) {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_to_bytes_respects_the_ceiling_and_char_boundaries() {
        // "é" is two bytes, "漢" three, "🦀" four.
        let cases: [(&str, usize, &str, bool); 9] = [
            ("hello", 5, "hello", false),
            ("hello", 9, "hello", false),
            ("hello", 3, "hel", true),
            ("hello", 0, "", true),
            ("aé", 2, "a", true),
            ("aé", 3, "aé", false),
            ("a漢b", 3, "a", true),
            ("a🦀", 4, "a", true),
            ("🦀", 3, "", true),
        ];
        for (input, max, expected, cut) in cases {
            let (kept, was_cut) = clip_to_bytes(input, max);
            assert_eq!((kept, was_cut), (expected, cut), "{input:?} max={max}");
            assert!(kept.len() <= max);
        }
    }

    #[test]
    fn clip_with_marker_never_exceeds_max() {
        let cases: [(&str, usize, &str, &str); 8] = [
            ("hello", 5, "...", "hello"),
            ("hello world", 8, "...", "hello..."),
            ("héllo wörld", 8, "...", "héll..."),
            ("a漢b漢c", 6, "...", "a..."),
            ("hello world", 3, "...", "..."),
            ("hello world", 2, "...", ".."),
            ("hello world", 0, "...", ""),
            ("hello world", 5, "", "hello"),
        ];
        for (input, max, marker, expected) in cases {
            let out = clip_with_marker(input, max, marker);
            assert_eq!(out, expected, "{input:?} max={max}");
            assert!(out.len() <= max, "{input:?} max={max} -> {out:?}");
        }
        assert!(matches!(
            clip_with_marker("fits", 4, "..."),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn truncate_to_bytes_reports_the_cut() {
        let mut text = String::from("a漢b");
        assert!(truncate_to_bytes(&mut text, 3));
        assert_eq!(text, "a");
        assert!(!truncate_to_bytes(&mut text, 3));
        assert_eq!(text, "a");
    }

    #[test]
    fn format_bytes_uses_binary_units_with_one_decimal() {
        let cases = [
            (0, "0 B"),
            (1023, "1023 B"),
            (1024, "1.0 KiB"),
            (1536, "1.5 KiB"),
            (2048, "2.0 KiB"),
            (1_047_552, "1023.0 KiB"),
            (1_048_575, "1.0 MiB"),
            (1_048_576, "1.0 MiB"),
            (2 * 1024 * 1024, "2.0 MiB"),
            (1_073_741_824, "1.0 GiB"),
            (5 * 1_073_741_824, "5.0 GiB"),
            (u64::MAX, "17179869184.0 GiB"),
        ];
        for (bytes, expected) in cases {
            assert_eq!(format_bytes(bytes), expected, "bytes={bytes}");
        }
    }
}
