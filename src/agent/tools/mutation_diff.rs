//! Bounded review data computed from the exact bytes a native tool published.
//!
//! This is result metadata, never a model-authored claim or a filesystem read
//! service. Producers publish it only after an identity/content-checked mutation
//! succeeds; UI and export consumers must never reopen the path.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) const MAX_DIFF_LINES: usize = 1_000;
pub(crate) const MAX_DIFF_BYTES: usize = 64 * 1024;
pub(crate) const MAX_INPUT_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const MAX_INPUT_LINES: usize = 100_000;
pub(crate) const MAX_RANGES: usize = 128;
const MAX_ROW_CHARS: usize = 500;

#[derive(Clone, Copy, Debug)]
pub(crate) struct DiffOptions {
    pub context: usize,
    pub max_lines: usize,
}
impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            context: 3,
            max_lines: 200,
        }
    }
}
impl DiffOptions {
    fn bounded(self) -> Self {
        Self {
            context: self.context.min(20),
            max_lines: self.max_lines.min(MAX_DIFF_LINES),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiffKind {
    Add,
    Delete,
    Context,
    Hunk,
    Notice,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DiffRow {
    pub kind: DiffKind,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AppliedRange {
    pub before_start: usize,
    pub before_end: usize,
    pub after_start: usize,
    pub after_end: usize,
}

/// True when `a` and `b` are equal after folding every `\r\n` to `\n`,
/// without allocating either folded copy.
fn eq_ignoring_crlf(a: &str, b: &str) -> bool {
    fn folded(s: &str) -> impl Iterator<Item = u8> + '_ {
        let bytes = s.as_bytes();
        bytes
            .iter()
            .enumerate()
            .filter(move |&(i, &byte)| !(byte == b'\r' && bytes.get(i + 1) == Some(&b'\n')))
            .map(|(_, &byte)| byte)
    }
    folded(a).eq(folded(b))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MutationDiff {
    pub path: String,
    /// None means an atomic create, not a missing/unauthorized prior read.
    pub before_sha256: Option<String>,
    pub after_sha256: String,
    pub before_bytes: usize,
    pub after_bytes: usize,
    pub ranges: Vec<AppliedRange>,
    pub rows: Vec<DiffRow>,
    pub omitted_rows: usize,
    pub clipped_rows: usize,
    pub content_unchanged: bool,
    #[serde(default)]
    pub imported: bool,
}

/// Read permission is not implied by edit/write authority. An optional
/// preview never asks for new access or performs another filesystem read.
pub(crate) fn preimage_allowed(
    permission: &Option<crate::agent::tools::PermCheck>,
    path: &str,
    contained: bool,
) -> bool {
    permission.as_ref().is_none_or(|permission| {
        permission
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .allows_mutation_preimage(path, contained)
    })
}

pub(crate) struct MutationOutcome {
    pub output: String,
    pub diff: Option<MutationDiff>,
}
impl MutationOutcome {
    pub(crate) fn into_return(self) -> rig::tool::ToolReturn<String> {
        let result = rig::tool::ToolReturn::success(self.output);
        match self.diff {
            Some(diff) => result.with_extension(diff),
            None => result,
        }
    }
}

fn digest(text: &str) -> String {
    crate::hex::encode_lower(Sha256::digest(text.as_bytes()))
}

#[derive(Clone, Copy)]
struct Line<'a> {
    start: usize,
    end: usize,
    text: &'a str,
}
fn lines(text: &str) -> Option<Vec<Line<'_>>> {
    let mut result = Vec::new();
    let mut start = 0;
    for raw in text.split_inclusive('\n') {
        if result.len() == MAX_INPUT_LINES {
            return None;
        }
        let end = start + raw.len();
        result.push(Line {
            start,
            end,
            text: raw,
        });
        start = end;
    }
    Some(result)
}

// Intern exact lines once. Matching cells compare integers rather than
// re-scanning long repeated strings. IDs depend only on encounter order.
fn line_identities<'a>(old: &[Line<'a>], new: &[Line<'a>]) -> (Vec<u32>, Vec<u32>) {
    let mut identities = std::collections::BTreeMap::<&'a str, u32>::new();
    let mut next_id = 0u32;
    let mut identify = |line: &Line<'a>| {
        *identities.entry(line.text).or_insert_with(|| {
            let id = next_id;
            next_id += 1;
            id
        })
    };
    (
        old.iter().map(&mut identify).collect(),
        new.iter().map(&mut identify).collect(),
    )
}

fn display_line(text: &str) -> &str {
    let without_lf = text.strip_suffix('\n').unwrap_or(text);
    if text.ends_with("\r\n") {
        without_lf.strip_suffix('\r').unwrap_or(without_lf)
    } else {
        without_lf
    }
}

fn window(lines: &[Line<'_>], start: usize, end: usize, context: usize) -> (usize, usize) {
    let mut first = lines.partition_point(|line| line.end <= start);
    // Appending bytes to an unterminated EOF line changes that same line.
    // Appending after a real newline is instead a zero-old-line insertion.
    if start == end
        && first == lines.len()
        && lines
            .last()
            .is_some_and(|line| line.end == start && !line.text.ends_with('\n'))
    {
        first = first.saturating_sub(1);
    }
    let last = if start == end {
        first + usize::from(lines.get(first).is_some_and(|line| line.start < start))
    } else {
        lines.partition_point(|line| line.start < end)
    };
    (
        first.saturating_sub(context),
        last.saturating_add(context).min(lines.len()),
    )
}

impl MutationDiff {
    /// Ranges are the tool's actual successful byte replacements, not SEARCH
    /// text or requested line numbers. They must be ordered and disjoint.
    pub(crate) fn build(
        path: &str,
        before: Option<&str>,
        after: &str,
        ranges: Vec<AppliedRange>,
        options: DiffOptions,
    ) -> Option<Self> {
        Self::build_with_digests(path, before, after, ranges, options, None, None)
    }

    /// Like `build`, reusing SHA-256 hex digests the caller already holds for
    /// exactly `before`/`after` instead of hashing the same bytes again.
    pub(crate) fn build_with_digests(
        path: &str,
        before: Option<&str>,
        after: &str,
        ranges: Vec<AppliedRange>,
        options: DiffOptions,
        before_sha256: Option<String>,
        after_sha256: Option<String>,
    ) -> Option<Self> {
        let options = options.bounded();
        let old = before.unwrap_or_default();
        if options.max_lines == 0
            || path.is_empty()
            || path.len() > 4096
            || old.len() > MAX_INPUT_BYTES
            || after.len() > MAX_INPUT_BYTES
            || ranges.len() > MAX_RANGES
        {
            return None;
        }
        if ranges.iter().any(|range| {
            range.before_start > range.before_end
                || range.before_end > old.len()
                || range.after_start > range.after_end
                || range.after_end > after.len()
                || !old.is_char_boundary(range.before_start)
                || !old.is_char_boundary(range.before_end)
                || !after.is_char_boundary(range.after_start)
                || !after.is_char_boundary(range.after_end)
        }) {
            return None;
        }
        if ranges.windows(2).any(|pair| {
            pair[0].before_end > pair[1].before_start || pair[0].after_end > pair[1].after_start
        }) {
            return None;
        }
        let old_lines = lines(old)?;
        let new_lines = lines(after)?;
        let (old_ids, new_ids) = line_identities(&old_lines, &new_lines);
        let mut result = Self {
            path: path.into(),
            before_sha256: before_sha256.or_else(|| before.map(digest)),
            after_sha256: after_sha256.unwrap_or_else(|| digest(after)),
            before_bytes: old.len(),
            after_bytes: after.len(),
            ranges,
            rows: Vec::new(),
            omitted_rows: 0,
            clipped_rows: 0,
            content_unchanged: before.is_some() && old == after,
            imported: false,
        };
        let mut total = 0usize;
        let mut retained_bytes = 0;
        let mut row = |kind, text: &str| {
            total += 1;
            if result.rows.len() >= options.max_lines || retained_bytes >= MAX_DIFF_BYTES {
                return;
            }
            // One stored row is one logical display row, including unusual
            // filenames and bare CR characters. File-supplied escapes remain
            // inert data until every display consumer sanitizes them.
            let text: std::borrow::Cow<'_, str> = if text.contains(['\r', '\n']) {
                std::borrow::Cow::Owned(text.replace('\r', "\\r").replace('\n', "\\n"))
            } else {
                std::borrow::Cow::Borrowed(text)
            };
            let clipped: String = text.chars().take(MAX_ROW_CHARS).collect();
            let clipped_flag = clipped.len() < text.len();
            let clipped = if clipped_flag {
                format!("{clipped}…")
            } else {
                clipped
            };
            if result.rows.len() < options.max_lines
                && retained_bytes.saturating_add(clipped.len()) <= MAX_DIFF_BYTES
            {
                retained_bytes += clipped.len();
                result.clipped_rows += usize::from(clipped_flag);
                result.rows.push(DiffRow {
                    kind,
                    text: clipped,
                });
            }
        };
        row(
            DiffKind::Notice,
            &format!("--- {}", if before.is_none() { "/dev/null" } else { path }),
        );
        row(DiffKind::Notice, &format!("+++ {path}"));
        if before.is_some() && old != after && eq_ignoring_crlf(old, after) {
            row(DiffKind::Notice, "Line endings changed (CRLF/LF)");
        }
        if result.content_unchanged {
            row(DiffKind::Notice, "No content change");
        } else if old.is_empty() && after.is_empty() {
            row(DiffKind::Notice, "Created empty file");
        } else {
            let mut windows: Vec<(usize, usize, usize, usize)> = Vec::new();
            for range in &result.ranges {
                let (os, oe) = window(
                    &old_lines,
                    range.before_start,
                    range.before_end,
                    options.context,
                );
                let (ns, ne) = window(
                    &new_lines,
                    range.after_start,
                    range.after_end,
                    options.context,
                );
                if let Some(previous) = windows.last_mut()
                    && (os <= previous.1 || ns <= previous.3)
                {
                    previous.1 = previous.1.max(oe);
                    previous.3 = previous.3.max(ne);
                } else {
                    windows.push((os, oe, ns, ne));
                }
            }
            let mut matching_budget = 2_000_000usize;
            for (mut os, mut oe, mut ns, mut ne) in windows {
                let (mut prefix, mut suffix) = (0, 0);
                while os + prefix < oe
                    && ns + prefix < ne
                    && old_ids[os + prefix] == new_ids[ns + prefix]
                {
                    prefix += 1;
                }
                while oe > os + prefix + suffix
                    && ne > ns + prefix + suffix
                    && old_ids[oe - 1 - suffix] == new_ids[ne - 1 - suffix]
                {
                    suffix += 1;
                }
                let trim_prefix = prefix.saturating_sub(options.context);
                let trim_suffix = suffix.saturating_sub(options.context);
                os += trim_prefix;
                ns += trim_prefix;
                oe -= trim_suffix;
                ne -= trim_suffix;
                prefix -= trim_prefix;
                suffix -= trim_suffix;
                row(
                    DiffKind::Hunk,
                    &format!(
                        "@@ -{},{} +{},{} @@",
                        if oe == os { os } else { os + 1 },
                        oe - os,
                        if ne == ns { ns } else { ns + 1 },
                        ne - ns
                    ),
                );
                let old_core = &old_lines[os + prefix..oe - suffix];
                let new_core = &new_lines[ns + prefix..ne - suffix];
                for line in &old_lines[os..os + prefix] {
                    row(DiffKind::Context, display_line(line.text));
                }
                let cells = old_core
                    .len()
                    .saturating_add(1)
                    .saturating_mul(new_core.len().saturating_add(1));
                if !old_core.is_empty() && !new_core.is_empty() && cells <= matching_budget {
                    matching_budget -= cells;
                    let width = new_core.len() + 1;
                    let mut matches = vec![0u32; cells];
                    for i in (0..old_core.len()).rev() {
                        for j in (0..new_core.len()).rev() {
                            matches[i * width + j] =
                                if old_ids[os + prefix + i] == new_ids[ns + prefix + j] {
                                    matches[(i + 1) * width + j + 1] + 1
                                } else {
                                    matches[(i + 1) * width + j].max(matches[i * width + j + 1])
                                };
                        }
                    }
                    let (mut i, mut j) = (0, 0);
                    while i < old_core.len() || j < new_core.len() {
                        if i < old_core.len()
                            && j < new_core.len()
                            && old_ids[os + prefix + i] == new_ids[ns + prefix + j]
                        {
                            row(DiffKind::Context, display_line(old_core[i].text));
                            i += 1;
                            j += 1;
                        } else if j == new_core.len()
                            || (i < old_core.len()
                                && matches[(i + 1) * width + j] >= matches[i * width + j + 1])
                        {
                            row(DiffKind::Delete, display_line(old_core[i].text));
                            if !old_core[i].text.ends_with('\n') {
                                row(DiffKind::Notice, "No newline at end of old file");
                            }
                            i += 1;
                        } else {
                            row(DiffKind::Add, display_line(new_core[j].text));
                            if !new_core[j].text.ends_with('\n') {
                                row(DiffKind::Notice, "No newline at end of new file");
                            }
                            j += 1;
                        }
                    }
                } else {
                    if !old_core.is_empty() && !new_core.is_empty() {
                        row(
                            DiffKind::Notice,
                            "Coarse replacement preview: line-matching work limit reached",
                        );
                    }
                    for line in old_core {
                        row(DiffKind::Delete, display_line(line.text));
                        if !line.text.ends_with('\n') {
                            row(DiffKind::Notice, "No newline at end of old file");
                        }
                    }
                    for line in new_core {
                        row(DiffKind::Add, display_line(line.text));
                        if !line.text.ends_with('\n') {
                            row(DiffKind::Notice, "No newline at end of new file");
                        }
                    }
                }
                for line in &new_lines[ne - suffix..ne] {
                    row(DiffKind::Context, display_line(line.text));
                }
            }
        }
        result.omitted_rows = total.saturating_sub(result.rows.len());
        Some(result)
    }

    pub(crate) fn replacement(
        path: &str,
        before: Option<&str>,
        after: &str,
        options: DiffOptions,
    ) -> Option<Self> {
        Self::build(
            path,
            before,
            after,
            vec![AppliedRange {
                before_start: 0,
                before_end: before.map_or(0, str::len),
                after_start: 0,
                after_end: after.len(),
            }],
            options,
        )
    }

    /// `replacement` reusing digests the caller already holds for exactly these bytes.
    pub(crate) fn replacement_with_digests(
        path: &str,
        before: Option<&str>,
        after: &str,
        options: DiffOptions,
        before_sha256: Option<String>,
        after_sha256: String,
    ) -> Option<Self> {
        Self::build_with_digests(
            path,
            before,
            after,
            vec![AppliedRange {
                before_start: 0,
                before_end: before.map_or(0, str::len),
                after_start: 0,
                after_end: after.len(),
            }],
            options,
            before_sha256,
            Some(after_sha256),
        )
    }

    #[cfg(any(feature = "acp", all(test, feature = "hooks", unix)))]
    pub(crate) fn review_text(&self) -> String {
        let mut text = String::new();
        for row in &self.rows {
            let prefix = match row.kind {
                DiffKind::Add => "+",
                DiffKind::Delete => "-",
                DiffKind::Context => " ",
                _ => "",
            };
            text.push_str(prefix);
            text.push_str(&row.text);
            text.push('\n');
        }
        if self.omitted_rows > 0 {
            text.push_str(&format!(
                "(+{} more diff rows omitted)\n",
                self.omitted_rows
            ));
        }
        if self.clipped_rows > 0 {
            text.push_str(&format!("({} long diff rows clipped)\n", self.clipped_rows));
        }
        text
    }

    pub(crate) fn bounded(&self) -> bool {
        fn valid_digest(value: &str) -> bool {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }
        !self.path.is_empty()
            && self.path.len() <= 4096
            && valid_digest(&self.after_sha256)
            && self.before_sha256.as_deref().is_none_or(valid_digest)
            && self.before_bytes <= MAX_INPUT_BYTES
            && self.after_bytes <= MAX_INPUT_BYTES
            && (self.before_sha256.is_some() || self.before_bytes == 0)
            && (!self.content_unchanged
                || (self.before_sha256.as_deref() == Some(self.after_sha256.as_str())
                    && self.before_bytes == self.after_bytes))
            && self.rows.len() <= MAX_DIFF_LINES
            && self.clipped_rows <= self.rows.len()
            && self.omitted_rows <= MAX_INPUT_LINES * 4 + MAX_RANGES * 8
            && self.ranges.len() <= MAX_RANGES
            && self.ranges.iter().all(|range| {
                range.before_start <= range.before_end
                    && range.before_end <= self.before_bytes
                    && range.after_start <= range.after_end
                    && range.after_end <= self.after_bytes
            })
            && self.ranges.windows(2).all(|pair| {
                pair[0].before_end <= pair[1].before_start
                    && pair[0].after_end <= pair[1].after_start
            })
            && self.rows.iter().all(|row| {
                !row.text.contains(['\n', '\r']) && row.text.chars().count() <= MAX_ROW_CHARS + 1
            })
            && self.rows.iter().map(|row| row.text.len()).sum::<usize>() <= MAX_DIFF_BYTES
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn crlf_fold_comparison_matches_replace() {
        for (a, b) in [
            ("a\r\nb", "a\nb"),
            ("a\r\nb", "a\rb"),
            ("a\r", "a"),
            ("a\r\n", "a\n"),
            ("\r\r\n", "\r\n"),
            ("x", "x"),
            ("", ""),
        ] {
            assert_eq!(
                super::eq_ignoring_crlf(a, b),
                a.replace("\r\n", "\n") == b.replace("\r\n", "\n"),
                "{a:?} {b:?}"
            );
        }
    }

    use super::*;
    #[test]
    fn actual_unicode_replacements_and_noop_are_distinguished() {
        let diff = MutationDiff::replacement(
            "é.rs",
            Some("α\nold\nω\n"),
            "α\nnew🙂\nω\n",
            DiffOptions::default(),
        )
        .unwrap();
        assert_eq!(
            diff.rows
                .iter()
                .filter(|row| row.kind == DiffKind::Delete)
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            ["old"]
        );
        assert_eq!(
            diff.rows
                .iter()
                .filter(|row| row.kind == DiffKind::Add)
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            ["new🙂"]
        );
        assert_ne!(
            diff.before_sha256.as_deref(),
            Some(diff.after_sha256.as_str())
        );
        let noop = MutationDiff::replacement("same", Some("same"), "same", DiffOptions::default())
            .unwrap();
        assert!(
            noop.content_unchanged
                && noop
                    .rows
                    .iter()
                    .all(|row| !matches!(row.kind, DiffKind::Add | DiffKind::Delete))
        );
    }
    #[test]
    fn new_files_missing_newlines_and_caps_are_explicit() {
        let created =
            MutationDiff::replacement("new", None, "last", DiffOptions::default()).unwrap();
        assert!(created.before_sha256.is_none());
        assert!(
            created
                .rows
                .iter()
                .any(|row| row.text == "No newline at end of new file")
        );
        let text = "🙂".repeat(4000) + "\n" + &"more\n".repeat(500);
        let limited = MutationDiff::replacement(
            "new",
            None,
            &text,
            DiffOptions {
                context: 3,
                max_lines: 5,
            },
        )
        .unwrap();
        assert_eq!(limited.rows.len(), 5);
        assert!(limited.omitted_rows > 0 && limited.clipped_rows > 0 && limited.bounded());
        assert!(
            MutationDiff::replacement(
                "large",
                None,
                &"x".repeat(MAX_INPUT_BYTES + 1),
                DiffOptions::default()
            )
            .is_none()
        );
    }
    #[test]
    fn disjoint_real_ranges_do_not_disclose_unrelated_middle_lines() {
        let before = "old\nprivate-middle\nold\n";
        let after = "new\nprivate-middle\nnew\n";
        let diff = MutationDiff::build(
            "path",
            Some(before),
            after,
            vec![
                AppliedRange {
                    before_start: 0,
                    before_end: 3,
                    after_start: 0,
                    after_end: 3,
                },
                AppliedRange {
                    before_start: 19,
                    before_end: 22,
                    after_start: 19,
                    after_end: 22,
                },
            ],
            DiffOptions {
                context: 0,
                max_lines: 200,
            },
        )
        .unwrap();
        assert_eq!(
            diff.rows
                .iter()
                .filter(|row| row.kind == DiffKind::Hunk)
                .count(),
            2
        );
        assert!(
            diff.rows
                .iter()
                .all(|row| !row.text.contains("private-middle"))
        );
    }
    #[test]
    fn line_endings_insertions_and_untrusted_bounds_are_explicit() {
        let crlf = MutationDiff::replacement(
            "file\nname",
            Some("one\r\ntwo\r\n"),
            "one\ntwo\n",
            DiffOptions::default(),
        )
        .unwrap();
        assert!(crlf.bounded());
        assert!(
            crlf.rows
                .iter()
                .any(|row| row.text == "Line endings changed (CRLF/LF)")
        );
        assert!(crlf.rows.iter().all(|row| !row.text.contains(['\n', '\r'])));
        let inserted = MutationDiff::build(
            "f",
            Some("ac\n"),
            "abc\n",
            vec![AppliedRange {
                before_start: 1,
                before_end: 1,
                after_start: 1,
                after_end: 2,
            }],
            DiffOptions {
                context: 0,
                max_lines: 200,
            },
        )
        .unwrap();
        assert!(
            inserted
                .rows
                .iter()
                .any(|row| row.kind == DiffKind::Delete && row.text == "ac")
        );
        assert!(
            inserted
                .rows
                .iter()
                .any(|row| row.kind == DiffKind::Add && row.text == "abc")
        );
        let eof = MutationDiff::build(
            "f",
            Some("a"),
            "ab",
            vec![AppliedRange {
                before_start: 1,
                before_end: 1,
                after_start: 1,
                after_end: 2,
            }],
            DiffOptions {
                context: 0,
                max_lines: 200,
            },
        )
        .unwrap();
        assert!(
            eof.rows
                .iter()
                .any(|row| row.kind == DiffKind::Delete && row.text == "a")
        );
        assert!(
            eof.rows
                .iter()
                .any(|row| row.kind == DiffKind::Add && row.text == "ab")
        );
        assert!(
            eof.rows
                .iter()
                .any(|row| row.text == "No newline at end of old file")
        );
        let newline_eof = MutationDiff::build(
            "f",
            Some("a\n"),
            "a\nb\n",
            vec![AppliedRange {
                before_start: 2,
                before_end: 2,
                after_start: 2,
                after_end: 4,
            }],
            DiffOptions {
                context: 0,
                max_lines: 200,
            },
        )
        .unwrap();
        assert!(
            newline_eof
                .rows
                .iter()
                .all(|row| row.kind != DiffKind::Delete)
        );
        assert!(
            newline_eof
                .rows
                .iter()
                .any(|row| row.kind == DiffKind::Hunk && row.text == "@@ -1,0 +2,1 @@")
        );
        assert!(
            newline_eof
                .rows
                .iter()
                .any(|row| row.kind == DiffKind::Add && row.text == "b")
        );
        let mut forged = inserted.clone();
        forged.after_sha256 = "not a digest".into();
        assert!(!forged.bounded());
        let mut forged = inserted.clone();
        forged.ranges[0].before_end = usize::MAX;
        assert!(!forged.bounded());
        let mut forged = inserted.clone();
        forged.rows[0].text = "multiple\nrows".into();
        assert!(!forged.bounded());
        let mut forged = inserted;
        forged.omitted_rows = usize::MAX;
        assert!(!forged.bounded());
        assert!(
            MutationDiff::build(
                "f",
                Some("é"),
                "x",
                vec![AppliedRange {
                    before_start: 1,
                    before_end: 2,
                    after_start: 0,
                    after_end: 1
                }],
                DiffOptions::default()
            )
            .is_none()
        );
    }

    #[test]
    fn matching_work_and_config_are_bounded_without_claiming_minimality() {
        let before = "old\n".repeat(2000);
        let after = "new\n".repeat(2000);
        let coarse = MutationDiff::replacement(
            "f",
            Some(&before),
            &after,
            DiffOptions {
                context: usize::MAX,
                max_lines: usize::MAX,
            },
        )
        .unwrap();
        assert_eq!(coarse.rows.len(), MAX_DIFF_LINES);
        assert!(
            coarse
                .rows
                .iter()
                .any(|row| row.text.contains("work limit reached"))
        );
        assert!(coarse.omitted_rows > 3000 && coarse.bounded());
        let cfg = crate::config::Config::default();
        assert_eq!(cfg.mutation_diff_options().context, 3);
        assert_eq!(cfg.mutation_diff_options().max_lines, 200);
        let cfg: crate::config::Config = serde_json::from_value(
            serde_json::json!({"diff_context_lines":9999,"max_diff_lines":9999}),
        )
        .unwrap();
        assert_eq!(cfg.mutation_diff_options().context, 20);
        assert_eq!(cfg.mutation_diff_options().max_lines, MAX_DIFF_LINES);
        assert!(
            MutationDiff::replacement(
                "f",
                None,
                "x",
                DiffOptions {
                    context: 3,
                    max_lines: 0
                }
            )
            .is_none()
        );
        assert!(
            MutationDiff::replacement(
                "f",
                None,
                &"\n".repeat(MAX_INPUT_LINES + 1),
                DiffOptions::default()
            )
            .is_none()
        );
    }

    fn permission(
        read: crate::permission::Action,
        workspace: &std::path::Path,
    ) -> crate::agent::tools::PermCheck {
        use crate::permission::{
            Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm,
        };
        std::sync::Arc::new(std::sync::Mutex::new(
            crate::permission::checker::PermissionChecker::new(
                &PermissionConfigs::from(PermissionConfig {
                    default: Some(Action::Allow),
                    read: Some(ToolPerm::Simple(read)),
                    ..Default::default()
                }),
                SecurityMode::Standard,
                Some(workspace.to_path_buf()),
                None,
            )
            .unwrap(),
        ))
    }

    #[tokio::test]
    async fn actual_tools_emit_only_successful_authorized_snapshots_and_keep_string_results() {
        use crate::agent::tools::{
            EditArgs, EditTool, ReadArgs, ReadTool, ReadTracker, WriteArgs, WriteTool,
        };
        use rig::tool::{Tool, ToolCallExtensions};
        let root = crate::tests::OwnedTestDirectory::new("mutation-native-tools");
        let tracker = ReadTracker::new(true);
        let read = ReadTool::new_with_tracker(None, None, None, 100, tracker.clone())
            .with_workspace(root.path());
        let write = WriteTool::new_with_tracker(None, None, None, tracker.clone())
            .with_workspace(root.path());
        let created = write
            .call_structured(
                WriteArgs {
                    path: "new.txt".into(),
                    content: "old🙂\n".into(),
                    overwrite: false,
                },
                &ToolCallExtensions::new(),
            )
            .await
            .unwrap();
        assert!(created.output.as_str().contains("Written"));
        let first = created.extensions.get::<MutationDiff>().unwrap();
        assert!(first.before_sha256.is_none());
        assert_eq!(first.after_sha256, digest("old🙂\n"));
        assert_eq!(
            std::fs::read_to_string(root.path().join("new.txt")).unwrap(),
            "old🙂\n"
        );
        assert!(
            write
                .call_structured(
                    WriteArgs {
                        path: "new.txt".into(),
                        content: "wrong\n".into(),
                        overwrite: false
                    },
                    &ToolCallExtensions::new()
                )
                .await
                .is_err()
        );
        read.call(ReadArgs {
            raw_notebook: false,
            path: "new.txt".into(),
            offset: None,
            limit: None,
        })
        .await
        .unwrap();
        let edited = EditTool::new_with_tracker(None, None, None, tracker.clone())
            .with_workspace(root.path())
            .call_structured(
                EditArgs {
                    path: "new.txt".into(),
                    replace_all: false,
                    fuzzy: false,
                    block: Some("<<<<<<< SEARCH\nold🙂\n=======\nnewα\n>>>>>>> REPLACE".into()),
                    file_crc: None,
                    edits: None,
                },
                &ToolCallExtensions::new(),
            )
            .await
            .unwrap();
        let diff = edited.extensions.get::<MutationDiff>().unwrap();
        assert_eq!(
            diff.before_sha256.as_deref(),
            Some(digest("old🙂\n").as_str())
        );
        assert_eq!(diff.after_sha256, digest("newα\n"));
        assert!(
            diff.rows
                .iter()
                .any(|row| row.kind == DiffKind::Delete && row.text == "old🙂")
        );
        assert!(
            diff.rows
                .iter()
                .any(|row| row.kind == DiffKind::Add && row.text == "newα")
        );
        assert!(edited.output.as_str().starts_with("Applied 1 edit(s)"));
        assert_eq!(
            std::fs::read_to_string(root.path().join("new.txt")).unwrap(),
            "newα\n"
        );
        // An already-read snapshot does not override a later read denial.
        read.call(ReadArgs {
            raw_notebook: false,
            path: "new.txt".into(),
            offset: None,
            limit: None,
        })
        .await
        .unwrap();
        let private_write = WriteTool::new_with_tracker(
            Some(permission(crate::permission::Action::Deny, root.path())),
            None,
            None,
            tracker.clone(),
        )
        .with_workspace(root.path());
        let private = private_write
            .call_structured(
                WriteArgs {
                    path: "new.txt".into(),
                    content: "replacement\n".into(),
                    overwrite: true,
                },
                &ToolCallExtensions::new(),
            )
            .await
            .unwrap();
        assert!(private.extensions.get::<MutationDiff>().is_none());
        assert_eq!(
            std::fs::read_to_string(root.path().join("new.txt")).unwrap(),
            "replacement\n"
        );
        read.call(ReadArgs {
            raw_notebook: false,
            path: "new.txt".into(),
            offset: None,
            limit: None,
        })
        .await
        .unwrap();
        let changed = write
            .call_structured(
                WriteArgs {
                    path: "new.txt".into(),
                    content: "replacement\n".into(),
                    overwrite: true,
                },
                &ToolCallExtensions::new(),
            )
            .await
            .unwrap();
        assert!(
            changed
                .extensions
                .get::<MutationDiff>()
                .unwrap()
                .content_unchanged
        );
        // Read denial suppresses both typed snapshots and ordinary excerpts.
        std::fs::write(
            root.path().join("private-edit.txt"),
            "LEGACY_PRIVATE_NEIGHBOR\nold\n",
        )
        .unwrap();
        let private_edit = EditTool::new_with_tracker(
            Some(permission(crate::permission::Action::Deny, root.path())),
            None,
            None,
            tracker.clone(),
        )
        .with_workspace(root.path())
        .call_structured(
            EditArgs {
                path: "private-edit.txt".into(),
                replace_all: false,
                fuzzy: false,
                block: Some("<<<<<<< SEARCH\nold\n=======\nnew\n>>>>>>> REPLACE".into()),
                file_crc: None,
                edits: None,
            },
            &ToolCallExtensions::new(),
        )
        .await
        .unwrap();
        assert!(private_edit.extensions.get::<MutationDiff>().is_none());
        assert!(!private_edit.output.contains("LEGACY_PRIVATE_NEIGHBOR"));
        assert!(private_edit.output.contains("new"));
        assert!(private_edit.output.starts_with("Applied 1 edit(s)"));
        // Legacy direct callers get the original string shape, not serialized metadata.
        let text = write
            .call(WriteArgs {
                path: "legacy.txt".into(),
                content: "plain\n".into(),
                overwrite: false,
            })
            .await
            .unwrap();
        assert!(text.starts_with("Written"));
        assert!(!text.contains("sha256"));
    }

    fn edit_block(search: &str, replacement: &str) -> crate::agent::tools::EditArgs {
        crate::agent::tools::EditArgs {
            path: "target.txt".into(),
            replace_all: false,
            fuzzy: false,
            block: Some(format!(
                "<<<<<<< SEARCH\n{search}\n=======\n{replacement}\n>>>>>>> REPLACE"
            )),
            file_crc: None,
            edits: None,
        }
    }

    #[tokio::test]
    async fn actual_edit_results_disclose_source_only_with_existing_read_authority() {
        use crate::agent::tools::{EditOp, EditTool, ReadTracker};
        use crate::permission::Action;
        use rig::tool::{Tool, ToolCallExtensions};
        for action in [Action::Deny, Action::Ask, Action::Allow] {
            let root = crate::tests::OwnedTestDirectory::new("edit-source-authority");
            let checker = permission(action, root.path());
            let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(8);
            let tool = EditTool::new_with_tracker(
                Some(checker),
                Some(ask_tx),
                None,
                ReadTracker::new(true),
            )
            .with_workspace(root.path());
            let allowed = action == Action::Allow;
            // Context-free line rendering is unsafe too: unchanged text on the
            // edited line and the surviving line after deletion are private.
            for (case, (source, search, replacement, fuzzy, replace_all, expected)) in [
                (
                    "PRIVATE_NEIGHBOR\nold\n",
                    "old",
                    "new",
                    false,
                    false,
                    "PRIVATE_NEIGHBOR\nnew\n",
                ),
                (
                    "PRIVATE_PREFIX old PRIVATE_SUFFIX\n",
                    "old",
                    "new",
                    false,
                    false,
                    "PRIVATE_PREFIX new PRIVATE_SUFFIX\n",
                ),
                (
                    "PRIVATE_NEIGHBOR\nold\n",
                    "old",
                    "",
                    false,
                    false,
                    "PRIVATE_NEIGHBOR\n\n",
                ),
                (
                    "PRIVATE_NEIGHBOR\n\tvalue = 100;\n",
                    "    value = 100;",
                    "new",
                    false,
                    false,
                    "PRIVATE_NEIGHBOR\nnew\n",
                ),
                (
                    "PRIVATE_NEIGHBOR\n    value = 100;\n",
                    "    value = 101;",
                    "new",
                    true,
                    false,
                    "PRIVATE_NEIGHBOR\nnew\n",
                ),
                (
                    "PRIVATE_NEIGHBOR\nold\nold\n",
                    "old",
                    "new",
                    false,
                    true,
                    "PRIVATE_NEIGHBOR\nnew\nnew\n",
                ),
            ]
            .into_iter()
            .enumerate()
            {
                let path = format!("target-{case}.txt");
                std::fs::write(root.path().join(&path), source).unwrap();
                let mut args = edit_block(search, replacement);
                args.path = path.clone();
                args.fuzzy = fuzzy;
                args.replace_all = replace_all;
                let result = tool
                    .call_structured(args, &ToolCallExtensions::new())
                    .await
                    .unwrap();
                assert!(result.output.starts_with("Applied "), "{}", result.output);
                assert_eq!(result.extensions.get::<MutationDiff>().is_some(), allowed);
                assert_eq!(
                    std::fs::read_to_string(root.path().join(&path)).unwrap(),
                    expected
                );
                assert_eq!(
                    result.output.contains("PRIVATE_"),
                    allowed,
                    "{}",
                    result.output
                );
                if !replacement.is_empty() {
                    assert!(result.output.contains(replacement));
                }
                if !allowed {
                    assert!(!result.output.contains("value = 100;"), "{}", result.output);
                    assert!(!result.output.contains("Resulting excerpt"));
                    assert!(result.output.contains("Replacement text"));
                }
            }
            let source = "PRIVATE_NEIGHBOR\nold\n";
            std::fs::write(root.path().join("target.txt"), source).unwrap();
            let result = tool
                .call_structured(
                    crate::agent::tools::EditArgs {
                        block: None,
                        file_crc: Some(crate::agent::tools::crc::crc32_hex(source.as_bytes())),
                        edits: Some(vec![EditOp {
                            line: Some(format!(
                                "2|{} old",
                                crate::agent::tools::crc::crc32_hex(b"old")
                            )),
                            lines: None,
                            text: "new".into(),
                        }]),
                        ..edit_block("old", "new")
                    },
                    &ToolCallExtensions::new(),
                )
                .await
                .unwrap();
            assert_eq!(result.output.contains("PRIVATE_NEIGHBOR"), allowed);
            assert_eq!(result.extensions.get::<MutationDiff>().is_some(), allowed);
            assert_eq!(
                std::fs::read_to_string(root.path().join("target.txt")).unwrap(),
                "PRIVATE_NEIGHBOR\nnew\n"
            );
            assert!(
                matches!(
                    ask_rx.try_recv(),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                ),
                "preview must never request read access"
            );
        }
    }

    #[tokio::test]
    async fn actual_edit_excerpt_authority_is_independent_of_diff_limits_and_prior_reads() {
        use crate::agent::tools::{EditTool, ReadArgs, ReadTool, ReadTracker};
        use crate::permission::Action;
        use rig::tool::{Tool, ToolCallExtensions};
        for (action, session_grant, disclose) in [
            (Action::Deny, true, false),
            (Action::Ask, false, false),
            (Action::Ask, true, true),
            (Action::Allow, false, true),
        ] {
            let root = crate::tests::OwnedTestDirectory::new("edit-preview-diff-limit");
            std::fs::write(root.path().join("target.txt"), "PRIVATE_NEIGHBOR\nold\n").unwrap();
            let tracker = ReadTracker::new(true);
            ReadTool::new_with_tracker(None, None, None, 100, tracker.clone())
                .with_workspace(root.path())
                .call(ReadArgs {
                    raw_notebook: false,
                    path: "target.txt".into(),
                    offset: None,
                    limit: None,
                })
                .await
                .unwrap();
            let checker = permission(action, root.path());
            if session_grant {
                checker
                    .lock()
                    .unwrap()
                    .add_session_allowlist("read".into(), "target.txt");
            }
            let result = EditTool::new_with_tracker(Some(checker), None, None, tracker)
                .with_workspace(root.path())
                .with_diff_options(DiffOptions {
                    context: 3,
                    max_lines: 0,
                })
                .call_structured(edit_block("old", "new"), &ToolCallExtensions::new())
                .await
                .unwrap();
            assert_eq!(
                result.output.contains("PRIVATE_NEIGHBOR"),
                disclose,
                "{}",
                result.output
            );
            assert!(result.output.contains("new"));
            assert!(
                result.extensions.get::<MutationDiff>().is_none(),
                "row limit disables metadata independently of string disclosure"
            );
            assert_eq!(
                std::fs::read_to_string(root.path().join("target.txt")).unwrap(),
                "PRIVATE_NEIGHBOR\nnew\n"
            );
        }
    }

    #[tokio::test]
    async fn actual_edit_failures_hide_source_candidates_and_current_checksums() {
        use crate::agent::tools::{EditOp, EditTool};
        use crate::permission::Action;
        use rig::tool::Tool;
        for action in [Action::Deny, Action::Ask, Action::Allow] {
            let root = crate::tests::OwnedTestDirectory::new("edit-failure-authority");
            let tool = EditTool::new(Some(permission(action, root.path())), None)
                .with_workspace(root.path());
            let allowed = action == Action::Allow;
            for (source, search, revealed, reason) in [
                (
                    "PRIVATE_A old\nPRIVATE_B old\n",
                    "old",
                    "PRIVATE_A",
                    "matched",
                ),
                (
                    "PRIVATE_NEIGHBOR\n    value = 100;\n",
                    "    value = 101;",
                    "value = 100;",
                    "match",
                ),
                (
                    "PRIVATE_A\n    value = 100;\nPRIVATE_B\n    value = 100;\n",
                    "    value = 101;",
                    "value = 100;",
                    "fuzzy matches",
                ),
                (
                    "PRIVATE_NEIGHBOR\n    value = 100;\nPRIVATE_TAIL\n",
                    "    value = 99999;",
                    "PRIVATE_TAIL",
                    "not found",
                ),
            ] {
                std::fs::write(root.path().join("target.txt"), source).unwrap();
                let message = tool
                    .call(edit_block(search, "new"))
                    .await
                    .unwrap_err()
                    .to_string();
                assert!(message.contains(reason), "{message}");
                assert_eq!(message.contains(revealed), allowed, "{message}");
                if !allowed {
                    assert!(!message.contains("PRIVATE_"), "{message}");
                }
                assert_eq!(
                    std::fs::read_to_string(root.path().join("target.txt")).unwrap(),
                    source
                );
            }
            let source = "PRIVATE_NEIGHBOR\nold\n";
            std::fs::write(root.path().join("target.txt"), source).unwrap();
            let crc = crate::agent::tools::crc::crc32_hex(source.as_bytes());
            let tag = crate::agent::tools::crc::crc32_hex(b"old");
            for (file_crc, supplied_tag, reason, current) in [
                (
                    "00000000".to_string(),
                    tag.clone(),
                    "File CRC mismatch",
                    crc.clone(),
                ),
                (crc.clone(), "00000000".to_string(), "Tag mismatch", tag),
            ] {
                let message = tool
                    .call(crate::agent::tools::EditArgs {
                        block: None,
                        file_crc: Some(file_crc),
                        edits: Some(vec![EditOp {
                            line: Some(format!("2|{supplied_tag} old")),
                            lines: None,
                            text: "new".into(),
                        }]),
                        ..edit_block("old", "new")
                    })
                    .await
                    .unwrap_err()
                    .to_string();
                assert!(message.contains(reason), "{message}");
                assert_eq!(message.contains(&current), allowed, "{message}");
                assert_eq!(
                    std::fs::read_to_string(root.path().join("target.txt")).unwrap(),
                    source
                );
            }
        }
    }

    #[tokio::test]
    async fn actual_sdk_edit_results_preserve_read_privacy_and_exact_metadata() {
        use crate::agent::tools::EditTool;
        use crate::permission::Action;
        use futures::StreamExt;
        use rig::agent::{
            AgentBuilder, AgentHook, Flow, HookContext, MultiTurnStreamItem, StepEvent,
        };
        use rig::completion::CompletionModel;
        use rig::streaming::{StreamedUserContent, StreamingChat};
        use rig::test_utils::{MockCompletionModel, MockStreamEvent};
        #[derive(Clone, Default)]
        struct Seen(std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>);
        impl<M: CompletionModel> AgentHook<M> for Seen {
            async fn on_event(&self, _: &HookContext, event: StepEvent<'_, M>) -> Flow {
                if let StepEvent::ToolResult {
                    internal_call_id,
                    result,
                    ..
                } = event
                {
                    self.0
                        .lock()
                        .unwrap()
                        .push((internal_call_id.into(), result.into()));
                }
                Flow::cont()
            }
        }
        for action in [Action::Deny, Action::Ask, Action::Allow] {
            let root = crate::tests::OwnedTestDirectory::new("edit-sdk-authority");
            std::fs::write(root.path().join("target.txt"), "PRIVATE_NEIGHBOR\nold\n").unwrap();
            #[cfg(not(all(feature = "hooks", unix)))]
            let checker = permission(action, root.path());
            #[cfg(all(feature = "hooks", unix))]
            let checker = {
                use crate::permission::{
                    PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm,
                };
                std::sync::Arc::new(std::sync::Mutex::new(
                    crate::permission::checker::PermissionChecker::new(
                        &PermissionConfigs::from(PermissionConfig {
                            default: Some(Action::Allow),
                            read: Some(ToolPerm::Simple(action)),
                            edit: Some(ToolPerm::Simple(Action::Ask)),
                            ..Default::default()
                        }),
                        SecurityMode::Standard,
                        Some(root.path().to_path_buf()),
                        None,
                    )
                    .unwrap(),
                ))
            };
            let tools = crate::agent::tools::memoize::definitions(vec![Box::new(
                EditTool::new(Some(checker.clone()), None).with_workspace(root.path()),
            )]);
            #[cfg(all(feature = "hooks", unix))]
            let tools = {
                use crate::extras::hooks::{
                    dispatcher::HookDispatcher,
                    settings::{HookGroup, HookHandler, HookTrust},
                };
                let dispatcher = std::sync::Arc::new(
                    HookDispatcher::from_config(&std::collections::HashMap::from([(
                        "PreToolUse".into(),
                        vec![HookGroup {
                            matcher: None,
                            hooks: vec![HookHandler {
                                kind: "command".into(),
                                command: Some("sh".into()),
                                args: Some(vec![
                                    "-c".into(),
                                    r#"printf '%s\n' '{"permissionDecision":"allow"}'"#.into(),
                                ]),
                                timeout: Some(2),
                                is_async: false,
                                condition: None,
                                once: false,
                                trust: HookTrust::Trusted,
                                env: Default::default(),
                            }],
                        }],
                    )]))
                    .unwrap(),
                );
                crate::extras::hooks::decorator::wrap_all(tools, dispatcher, Some(checker), None)
            };
            let model = MockCompletionModel::from_stream_turns(vec![
                vec![
                    MockStreamEvent::tool_call(
                        "provider-edit",
                        "edit",
                        serde_json::json!({"path":"target.txt","block":"<<<<<<< SEARCH\nold\n=======\nnew\n>>>>>>> REPLACE"}),
                    ),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
                vec![
                    MockStreamEvent::text("done"),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
            ]);
            let seen = Seen::default();
            let agent = AgentBuilder::new(model)
                .tools(tools)
                .add_hook(seen.clone())
                .add_hook(MutationDiffHook)
                .build();
            let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
            let mut returned = Vec::new();
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                scope_events(sender, async {
                    let mut stream = agent
                        .stream_chat("edit", Vec::<rig::completion::Message>::new())
                        .max_turns(2)
                        .await;
                    while let Some(item) = stream.next().await {
                        if let MultiTurnStreamItem::StreamUserItem(
                            StreamedUserContent::ToolResult {
                                tool_result,
                                internal_call_id,
                            },
                        ) = item.unwrap()
                        {
                            assert_eq!(tool_result.id, "provider-edit");
                            let text = tool_result
                                .content
                                .iter()
                                .map(|content| match content {
                                    rig::completion::message::ToolResultContent::Text(text) => {
                                        text.text.as_str()
                                    }
                                    _ => panic!("edit returns text"),
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            returned.push((internal_call_id, text));
                        }
                    }
                }),
            )
            .await
            .unwrap();
            let seen = seen.0.lock().unwrap();
            assert_eq!(seen.len(), 1);
            assert_eq!(
                returned, *seen,
                "streamed model-visible result must match the exact SDK result"
            );
            assert!(seen[0].1.starts_with("Applied 1 edit(s)"));
            assert_eq!(
                seen[0].1.contains("PRIVATE_NEIGHBOR"),
                action == Action::Allow
            );
            let event = receiver.try_recv();
            if action == Action::Allow {
                let crate::event::AgentEvent::FileMutation { id, name, diff } = event.unwrap()
                else {
                    panic!("expected exact mutation metadata")
                };
                assert_eq!(id, seen[0].0);
                assert_eq!(name, "edit");
                assert_eq!(
                    diff.before_sha256.as_deref(),
                    Some(digest("PRIVATE_NEIGHBOR\nold\n").as_str())
                );
                assert_eq!(diff.after_sha256, digest("PRIVATE_NEIGHBOR\nnew\n"));
            } else {
                assert!(
                    event.is_err(),
                    "no typed source metadata without read authority"
                );
            }
            assert_eq!(
                std::fs::read_to_string(root.path().join("target.txt")).unwrap(),
                "PRIVATE_NEIGHBOR\nnew\n"
            );
        }
    }

    #[tokio::test]
    async fn actual_sdk_mutations_are_bound_to_exact_invocations_and_observer_scope() {
        use futures::StreamExt;
        use rig::agent::{AgentBuilder, AgentHook, Flow, HookContext, StepEvent};
        use rig::completion::CompletionModel;
        use rig::streaming::StreamingChat;
        use rig::test_utils::{MockCompletionModel, MockStreamEvent};
        #[derive(Clone, Default)]
        struct Calls(std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>);
        impl<M: CompletionModel> AgentHook<M> for Calls {
            async fn on_event(&self, _: &HookContext, event: StepEvent<'_, M>) -> Flow {
                if let StepEvent::ToolCall {
                    tool_call_id,
                    internal_call_id,
                    ..
                } = event
                {
                    self.0.lock().unwrap().insert(
                        tool_call_id.expect("mock provider ID").into(),
                        internal_call_id.into(),
                    );
                }
                Flow::cont()
            }
        }
        let root = crate::tests::OwnedTestDirectory::new("mutation-sdk");
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::tool_call(
                    "provider-a",
                    "write",
                    serde_json::json!({"path":"a.txt","content":"alpha\n"}),
                )
                .with_call_id("provider-a"),
                MockStreamEvent::tool_call(
                    "provider-b",
                    "write",
                    serde_json::json!({"path":"b.txt","content":"beta\n"}),
                )
                .with_call_id("provider-b"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let tools = crate::agent::tools::memoize::definitions(vec![Box::new(
            crate::agent::tools::WriteTool::new(None, None, None).with_workspace(root.path()),
        )]);
        let tools = crate::agent::tools::concurrency::bind(tools);
        let calls = Calls::default();
        let agent = AgentBuilder::new(model)
            .tools(tools)
            .add_hook(calls.clone())
            .add_hook(MutationDiffHook)
            .build();
        let records = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = records.clone();
        let observer: MutationObserver = std::sync::Arc::new(move |id, name, diff| {
            observed
                .lock()
                .unwrap()
                .push((id.to_string(), name.to_string(), diff.clone()))
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            scope_observer(observer, async {
                let mut stream = agent
                    .stream_chat("write both", Vec::<rig::completion::Message>::new())
                    .max_turns(2)
                    .tool_concurrency(2)
                    .await;
                while let Some(item) = stream.next().await {
                    item.unwrap();
                }
            }),
        )
        .await
        .unwrap();
        let records = records.lock().unwrap();
        assert_eq!(records.len(), 2);
        for (provider, path, content) in [
            ("provider-a", "a.txt", "alpha\n"),
            ("provider-b", "b.txt", "beta\n"),
        ] {
            let id = calls.0.lock().unwrap().get(provider).unwrap().clone();
            let (_, name, diff) = records.iter().find(|(actual, _, _)| actual == &id).unwrap();
            assert_eq!(name, "write");
            assert!(diff.path.ends_with(path));
            assert_eq!(diff.after_sha256, digest(content));
            assert_eq!(
                std::fs::read_to_string(root.path().join(path)).unwrap(),
                content
            );
        }
        assert!(MUTATION_EVENTS.try_with(Clone::clone).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn actual_ambient_symlink_mutations_record_retained_canonical_identity() {
        use crate::agent::tools::{
            EditArgs, EditTool, ReadArgs, ReadTool, ReadTracker, WriteArgs, WriteTool,
        };
        use rig::tool::{Tool, ToolCallExtensions};
        let root = crate::tests::OwnedTestDirectory::new("mutation-ambient-identity");
        let target = root.path().join("actual.txt");
        let alias = root.path().join("alias.txt");
        std::fs::write(&target, "original\n").unwrap();
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        let tracker = ReadTracker::new(true);
        ReadTool::new_with_tracker(None, None, None, 100, tracker.clone())
            .call(ReadArgs {
                raw_notebook: false,
                path: alias.to_string_lossy().into_owned(),
                offset: None,
                limit: None,
            })
            .await
            .unwrap();
        let written = WriteTool::new_with_tracker(None, None, None, tracker.clone())
            .call_structured(
                WriteArgs {
                    path: alias.to_string_lossy().into_owned(),
                    content: "written\n".into(),
                    overwrite: true,
                },
                &ToolCallExtensions::new(),
            )
            .await
            .unwrap();
        let metadata = written.extensions.get::<MutationDiff>().unwrap();
        assert_eq!(metadata.path, target.to_string_lossy());
        assert_eq!(metadata.after_sha256, digest("written\n"));
        assert!(
            written.output.contains(alias.to_str().unwrap()),
            "legacy summary still names requested alias"
        );
        assert_eq!(std::fs::read_link(&alias).unwrap(), target);
        let edited = EditTool::new_with_tracker(None, None, None, tracker)
            .call_structured(
                EditArgs {
                    path: alias.to_string_lossy().into_owned(),
                    replace_all: false,
                    fuzzy: false,
                    block: Some("<<<<<<< SEARCH\nwritten\n=======\nedited\n>>>>>>> REPLACE".into()),
                    file_crc: None,
                    edits: None,
                },
                &ToolCallExtensions::new(),
            )
            .await
            .unwrap();
        let metadata = edited.extensions.get::<MutationDiff>().unwrap();
        assert_eq!(metadata.path, target.to_string_lossy());
        assert_eq!(metadata.after_sha256, digest("edited\n"));
        assert!(edited.output.contains(alias.to_str().unwrap()));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "edited\n");
        assert_eq!(std::fs::read_link(&alias).unwrap(), target);
    }

    #[cfg(all(feature = "hooks", unix))]
    #[tokio::test]
    async fn actual_hooked_sdk_write_grants_and_redaction_preserve_read_authority_and_real_bytes() {
        use crate::agent::tools::{ReadArgs, ReadTool, ReadTracker, WriteTool};
        use crate::extras::hooks::{
            decorator::wrap_all,
            dispatcher::HookDispatcher,
            settings::{HookGroup, HookHandler, HookTrust},
        };
        use crate::permission::{
            Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm,
        };
        use futures::StreamExt;
        use rig::agent::{AgentBuilder, AgentHook, Flow, HookContext, StepEvent};
        use rig::completion::CompletionModel;
        use rig::streaming::StreamingChat;
        use rig::test_utils::{MockCompletionModel, MockStreamEvent};
        use rig::tool::Tool;
        fn handler(command: &str) -> HookHandler {
            HookHandler {
                kind: "command".into(),
                command: Some("sh".into()),
                args: Some(vec!["-c".into(), command.into()]),
                timeout: Some(2),
                is_async: false,
                condition: None,
                once: false,
                trust: HookTrust::Trusted,
                env: Default::default(),
            }
        }
        #[derive(Clone, Default)]
        struct Seen(std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>);
        impl<M: CompletionModel> AgentHook<M> for Seen {
            async fn on_event(&self, _: &HookContext, event: StepEvent<'_, M>) -> Flow {
                if let StepEvent::ToolResult {
                    internal_call_id,
                    result,
                    ..
                } = event
                {
                    self.0
                        .lock()
                        .unwrap()
                        .push((internal_call_id.into(), result.into()));
                }
                Flow::cont()
            }
        }
        for (read_action, write_denied) in [
            (Action::Allow, false),
            (Action::Ask, false),
            (Action::Deny, false),
            (Action::Allow, true),
        ] {
            let root = crate::tests::OwnedTestDirectory::new("mutation-hooked-sdk");
            std::fs::write(root.path().join("target.txt"), "PRIVATE_BEFORE\n").unwrap();
            let tracker = ReadTracker::new(true);
            ReadTool::new_with_tracker(None, None, None, 100, tracker.clone())
                .with_workspace(root.path())
                .call(ReadArgs {
                    raw_notebook: false,
                    path: "target.txt".into(),
                    offset: None,
                    limit: None,
                })
                .await
                .unwrap();
            let checker = std::sync::Arc::new(std::sync::Mutex::new(
                crate::permission::checker::PermissionChecker::new(
                    &PermissionConfigs::from(PermissionConfig {
                        read: Some(ToolPerm::Simple(read_action)),
                        write: Some(ToolPerm::Simple(if write_denied {
                            Action::Deny
                        } else {
                            Action::Ask
                        })),
                        ..Default::default()
                    }),
                    SecurityMode::Standard,
                    Some(root.path().to_path_buf()),
                    None,
                )
                .unwrap(),
            ));
            let dispatcher=std::sync::Arc::new(HookDispatcher::from_config(&std::collections::HashMap::from([
                ("PreToolUse".into(),vec![HookGroup {matcher:None,hooks:vec![handler(r#"printf '%s\n' '{"permissionDecision":"allow","updatedInput":{"path":"target.txt","content":"ACTUAL_HOOK_BYTES\n","overwrite":true}}'"#)]}]),
                ("PostToolUse".into(),vec![HookGroup {matcher:None,hooks:vec![handler(r#"printf '%s\n' '{"redactions":["Written"]}'"#)]}]),
            ])).unwrap());
            let tools = crate::agent::tools::memoize::definitions(vec![Box::new(
                WriteTool::new_with_tracker(Some(checker.clone()), None, None, tracker)
                    .with_workspace(root.path()),
            )]);
            let tools = wrap_all(tools, dispatcher, Some(checker), None);
            let tools = crate::agent::tools::concurrency::bind(tools);
            let model = MockCompletionModel::from_stream_turns(vec![
                vec![
                    MockStreamEvent::tool_call(
                        "provider-real-write",
                        "write",
                        serde_json::json!({"path":"target.txt","content":"MODEL_CLAIM\n","overwrite":true}),
                    ),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
                vec![
                    MockStreamEvent::text("done"),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
            ]);
            let seen = Seen::default();
            let agent = AgentBuilder::new(model)
                .tools(tools)
                .add_hook(seen.clone())
                .add_hook(MutationDiffHook)
                .build();
            let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
            tokio::time::timeout(
                std::time::Duration::from_secs(8),
                scope_events(sender, async {
                    let mut stream = agent
                        .stream_chat("write", Vec::<rig::completion::Message>::new())
                        .max_turns(2)
                        .await;
                    while let Some(item) = stream.next().await {
                        item.unwrap();
                    }
                }),
            )
            .await
            .unwrap();
            let bytes = std::fs::read_to_string(root.path().join("target.txt")).unwrap();
            let seen = seen.0.lock().unwrap();
            assert_eq!(seen.len(), 1);
            if write_denied {
                assert_eq!(bytes, "PRIVATE_BEFORE\n");
            } else {
                assert_eq!(bytes, "ACTUAL_HOOK_BYTES\n");
                assert!(seen[0].1.contains("[REDACTED]"));
            }
            if read_action == Action::Allow && !write_denied {
                let crate::event::AgentEvent::FileMutation { id, name, diff } =
                    receiver.try_recv().unwrap()
                else {
                    panic!("typed mutation event")
                };
                assert_eq!(id.as_str(), seen[0].0);
                assert_eq!(name, "write");
                assert_eq!(diff.after_sha256, digest(&bytes));
                assert!(
                    diff.review_text().contains("-PRIVATE_BEFORE")
                        && diff.review_text().contains("+ACTUAL_HOOK_BYTES")
                );
                assert!(!diff.review_text().contains("MODEL_CLAIM"));
            }
            assert!(
                receiver.try_recv().is_err(),
                "denied/read-unapproved mutations cannot publish metadata"
            );
        }
    }
}

// The typed SDK result already binds data to one concrete tool invocation.
// This run-local sink transports that exact metadata; there is no registry,
// name-based FIFO, filename lookup, or model-output parser.
pub(crate) type MutationObserver = std::sync::Arc<dyn Fn(&str, &str, &MutationDiff) + Send + Sync>;
#[derive(Clone)]
enum MutationSink {
    Events(tokio::sync::mpsc::Sender<crate::event::AgentEvent>),
    Observer(MutationObserver),
}
tokio::task_local! {
    static MUTATION_EVENTS: MutationSink;
}

pub(crate) async fn scope_events<F: std::future::Future>(
    sender: tokio::sync::mpsc::Sender<crate::event::AgentEvent>,
    future: F,
) -> F::Output {
    MUTATION_EVENTS
        .scope(MutationSink::Events(sender), future)
        .await
}

/// The interactive event sender scoped around the current agent run, if any.
/// Headless observers have no live event stream and yield `None`.
pub(crate) fn scoped_event_sender() -> Option<tokio::sync::mpsc::Sender<crate::event::AgentEvent>> {
    MUTATION_EVENTS
        .try_with(|sink| match sink {
            MutationSink::Events(sender) => Some(sender.clone()),
            MutationSink::Observer(_) => None,
        })
        .ok()
        .flatten()
}

/// Headless callers supply the same output-policy observer that owns their
/// normal event sinks. It receives one bounded typed snapshot with exact IDs;
/// the caller retains output failure/cancellation policy and owns its cleanup.
pub(crate) async fn scope_observer<F: std::future::Future>(
    observer: MutationObserver,
    future: F,
) -> F::Output {
    MUTATION_EVENTS
        .scope(MutationSink::Observer(observer), future)
        .await
}

#[derive(Clone, Copy)]
pub(crate) struct MutationDiffHook;

impl<M: rig::completion::CompletionModel> rig::agent::AgentHook<M> for MutationDiffHook {
    async fn on_event(
        &self,
        _context: &rig::agent::HookContext,
        event: rig::agent::StepEvent<'_, M>,
    ) -> rig::agent::Flow {
        if let rig::agent::StepEvent::ToolResult {
            tool_name,
            internal_call_id,
            outcome,
            extensions,
            ..
        } = event
            && matches!(tool_name, "edit" | "write" | "notebook_edit")
            && matches!(outcome, rig::tool::ToolOutcome::Success)
            && let Some(diff) = extensions.get::<MutationDiff>()
            && diff.bounded()
            && let Ok(sink) = MUTATION_EVENTS.try_with(Clone::clone)
        {
            match sink {
                MutationSink::Events(sender) => {
                    let _ = sender
                        .send(crate::event::AgentEvent::FileMutation {
                            id: internal_call_id.into(),
                            name: tool_name.into(),
                            diff: diff.clone(),
                        })
                        .await;
                }
                MutationSink::Observer(observer) => observer(internal_call_id, tool_name, diff),
            }
        }
        rig::agent::Flow::cont()
    }

    fn observes(&self, kind: rig::agent::StepEventKind) -> bool {
        matches!(kind, rig::agent::StepEventKind::ToolResult)
    }
}
