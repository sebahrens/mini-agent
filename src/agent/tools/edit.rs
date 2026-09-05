use rig::tool::Tool;
use tokio::io::AsyncReadExt;

use crate::agent::tools::crc::crc32_hex;
use crate::agent::tools::normalize::NormalizedText;
use crate::agent::tools::{
    AskSender, EditArgs, EditBlock, EditOp, PermCheck, ReadTracker, ToolError,
    check_perm_bound_path, check_perm_path, edit_system, levenshtein_similarity,
    normalize_whitespace,
};
use crate::config::types::EditSystem;
#[cfg(feature = "lsp")]
use crate::extras::lsp::LspManager;

pub struct EditTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    workspace: Option<std::sync::Arc<crate::paths::WorkspaceBinding>>,
    read_tracker: ReadTracker,
    /// When `Some`, edited files are synced to their language server and
    /// fresh diagnostics are appended to the tool result.
    #[cfg(feature = "lsp")]
    pub lsp: Option<LspManager>,
}

impl EditTool {
    #[cfg(test)]
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>) -> Self {
        Self::new_with_tracker(permission, ask_tx, ReadTracker::new(true))
    }

    pub(crate) fn new_with_tracker(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        read_tracker: ReadTracker,
    ) -> Self {
        EditTool {
            permission,
            ask_tx,
            workspace: None,
            read_tracker,
            #[cfg(feature = "lsp")]
            lsp: None,
        }
    }

    pub(crate) fn with_workspace_binding(
        mut self,
        workspace: std::sync::Arc<crate::paths::WorkspaceBinding>,
    ) -> Self {
        self.workspace = Some(workspace);
        self
    }

    pub(crate) fn with_workspace(self, root: impl Into<std::path::PathBuf>) -> Self {
        self.with_workspace_binding(crate::agent::tools::capture_workspace_binding(root.into()))
    }

    #[cfg(feature = "lsp")]
    pub fn with_lsp(mut self, lsp: Option<LspManager>) -> Self {
        self.lsp = lsp;
        self
    }
}

// ── V1: Similarity (SEARCH/REPLACE) ──────────────────────────────────────

fn parse_blocks(raw: &str) -> Result<Vec<EditBlock>, ToolError> {
    let mut blocks = Vec::new();
    let mut in_block = false;
    let mut search_lines: Vec<String> = Vec::new();
    let mut replace_lines: Vec<String> = Vec::new();
    let mut phase: u8 = 0;

    for line in raw.lines() {
        match line.trim() {
            "<<<<<<< SEARCH" => {
                if in_block {
                    return Err(ToolError::Msg(
                        "Nested SEARCH/REPLACE block detected. Close each block with >>>>>>> REPLACE before starting a new one.".to_string(),
                    ));
                }
                in_block = true;
                search_lines.clear();
                replace_lines.clear();
                phase = 1;
            }
            "=======" if phase == 1 => {
                phase = 2;
            }
            ">>>>>>> REPLACE" if phase == 2 => {
                let search = search_lines.join("\n");
                if search.is_empty() {
                    return Err(ToolError::Msg(format!(
                        "Block {} has empty search text. Each block must have a non-empty SEARCH section.",
                        blocks.len() + 1
                    )));
                }
                blocks.push(EditBlock {
                    search,
                    replace: replace_lines.join("\n"),
                });
                in_block = false;
                phase = 0;
            }
            _ if phase == 1 => {
                search_lines.push(line.to_string());
            }
            _ if phase == 2 => {
                replace_lines.push(line.to_string());
            }
            _ => {}
        }
    }

    if in_block {
        return Err(ToolError::Msg(
            "Unclosed SEARCH/REPLACE block. Each block must end with >>>>>>> REPLACE.".to_string(),
        ));
    }

    if blocks.is_empty() {
        return Err(ToolError::Msg(
            "No SEARCH/REPLACE blocks found. Use format:\n<<<<<<< SEARCH\nexisting code to find\n=======\nreplacement code\n>>>>>>> REPLACE\n\nMultiple blocks can be included for editing different parts of the same file."
                .to_string(),
        ));
    }

    Ok(blocks)
}

enum MatchResult {
    Exact(usize),
    Normalized(usize, usize),
    AmbiguousNormalized(usize, usize),
    FuzzyApply(usize, usize, f64),
    AmbiguousFuzzy((usize, f64), (usize, f64)),
    FuzzySuggest(usize, f64, String),
    NotFound,
}

const MAX_FUZZY_DISTANCE_CELLS: u128 = 8_000_000;

#[derive(Clone, Copy)]
struct LineSpan<'a> {
    start: usize,
    content_end: usize,
    full_end: usize,
    text: &'a str,
}

fn line_spans(content: &str) -> Vec<LineSpan<'_>> {
    let mut spans = Vec::new();
    let mut start = 0usize;
    for line in content.split_inclusive('\n') {
        let full_end = start + line.len();
        let without_lf = line.strip_suffix('\n').unwrap_or(line);
        let text = without_lf.strip_suffix('\r').unwrap_or(without_lf);
        spans.push(LineSpan {
            start,
            content_end: start + text.len(),
            full_end,
            text,
        });
        start = full_end;
    }
    if start < content.len() {
        let line = &content[start..];
        spans.push(LineSpan {
            start,
            content_end: content.len(),
            full_end: content.len(),
            text: line.strip_suffix('\r').unwrap_or(line),
        });
    }
    spans
}

fn find_best_match(content: &str, search: &str) -> MatchResult {
    // Step 1: exact match in original content
    if let Some(pos) = content.find(search) {
        return MatchResult::Exact(pos);
    }

    // Step 2: normalized match in full text. Normalization drops and merges
    // bytes (trailing whitespace, collapsed blank lines, tab expansion), so the
    // match span is translated back through the normalizer's byte map rather
    // than by arithmetic on the original text.
    let content_norm = NormalizedText::new(content);
    let search_norm = normalize_whitespace(search);
    if search_norm.is_empty() {
        return MatchResult::NotFound;
    }
    let mut normalized_matches = content_norm
        .text
        .match_indices(&search_norm)
        .map(|(norm_pos, _)| content_norm.source_range(norm_pos, search_norm.len()));
    let first_normalized = normalized_matches.next();
    if let (Some((first, _)), Some((second, _))) = (first_normalized, normalized_matches.next()) {
        return MatchResult::AmbiguousNormalized(
            content[..first].matches('\n').count() + 1,
            content[..second].matches('\n').count() + 1,
        );
    }
    if let Some((byte_start, byte_end)) = first_normalized {
        return MatchResult::Normalized(byte_start, byte_end);
    }

    // Step 3: fuzzy line-level matching
    let search_lines: Vec<&str> = search.lines().collect();
    let spans = line_spans(content);
    let content_lines: Vec<&str> = spans.iter().map(|span| span.text).collect();

    if search_lines.is_empty() || content_lines.len() < search_lines.len() {
        return MatchResult::NotFound;
    }

    let search_norm_lines: Vec<String> = search_lines
        .iter()
        .map(|l| normalize_whitespace(l))
        .collect();
    let search_norm_joined = search_norm_lines.join("\n");
    let content_norm_lines: Vec<String> = content_lines
        .iter()
        .map(|line| normalize_whitespace(line))
        .collect();

    let window_size = search_lines.len();
    let candidate_count = content_lines.len() - window_size + 1;
    let search_chars = search_norm_joined.chars().count() as u128;
    let line_chars: Vec<u128> = content_norm_lines
        .iter()
        .map(|line| line.chars().count() as u128)
        .collect();
    let mut window_chars =
        line_chars[..window_size].iter().sum::<u128>() + window_size.saturating_sub(1) as u128;
    let mut fuzzy_cells = 0u128;
    for start in 0..candidate_count {
        fuzzy_cells = fuzzy_cells.saturating_add(search_chars.saturating_mul(window_chars));
        if fuzzy_cells > MAX_FUZZY_DISTANCE_CELLS {
            return MatchResult::NotFound;
        }
        if start + window_size < line_chars.len() {
            window_chars = window_chars
                .saturating_sub(line_chars[start])
                .saturating_add(line_chars[start + window_size]);
        }
    }

    let mut best_sim = 0.0f64;
    let mut best_start = 0usize;
    let mut applicable = None;

    for start in 0..=content_lines.len() - search_lines.len() {
        let window_norm = content_norm_lines[start..start + search_lines.len()].join("\n");
        let sim = levenshtein_similarity(&search_norm_joined, &window_norm);
        if sim > best_sim {
            best_sim = sim;
            best_start = start;
        }
        if sim >= 0.85 {
            if let Some((first_start, first_sim)) = applicable {
                return MatchResult::AmbiguousFuzzy((first_start + 1, first_sim), (start + 1, sim));
            }
            applicable = Some((start, sim));
        }
    }

    if let Some((start, sim)) = applicable {
        let byte_start = spans[start].start;
        let byte_end = spans[start + search_lines.len() - 1].content_end;
        MatchResult::FuzzyApply(byte_start, byte_end, sim)
    } else if best_sim >= 0.60 {
        let preview: String = content_lines[best_start..]
            .iter()
            .take(3)
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        MatchResult::FuzzySuggest(best_start + 1, best_sim, preview)
    } else {
        MatchResult::NotFound
    }
}

fn count_exact_matches(content: &str, search: &str) -> usize {
    content.match_indices(search).count()
}

fn dominant_line_ending(content: &str) -> &'static str {
    let crlf = content
        .as_bytes()
        .windows(2)
        .filter(|w| *w == b"\r\n")
        .count();
    let total_lf = content
        .as_bytes()
        .iter()
        .filter(|byte| **byte == b'\n')
        .count();
    let bare_lf = total_lf.saturating_sub(crlf);
    if crlf > bare_lf { "\r\n" } else { "\n" }
}

fn with_line_ending(text: &str, line_ending: &str) -> String {
    let normalized = text.replace("\r\n", "\n");
    if line_ending == "\n" {
        normalized
    } else {
        normalized.replace('\n', line_ending)
    }
}

fn bounded_match_excerpt(content: &str, start: usize, end: usize) -> String {
    const MAX_CHARS: usize = 240;
    let matched = &content[start..end];
    let mut chars = matched.chars();
    let excerpt: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{excerpt}…")
    } else {
        excerpt
    }
}

async fn handle_similarity(
    path: &str,
    block: &str,
    content: &str,
) -> Result<(Vec<String>, Vec<(usize, usize, String)>), ToolError> {
    let blocks = parse_blocks(block)?;
    let line_ending = dominant_line_ending(content);

    struct ResolvedSim {
        byte_start: usize,
        byte_end: usize,
        replace: String,
        note: String,
    }

    let mut resolved: Vec<ResolvedSim> = Vec::new();

    for (i, blk) in blocks.iter().enumerate() {
        let label = if blocks.len() > 1 {
            format!("Block {}: ", i + 1)
        } else {
            String::new()
        };

        let search = with_line_ending(&blk.search, line_ending);
        let replace = with_line_ending(&blk.replace, line_ending);
        match find_best_match(content, &search) {
            MatchResult::Exact(pos) => {
                let count = count_exact_matches(content, &search);
                if count > 1 {
                    let line_starts: Vec<usize> = std::iter::once(0)
                        .chain(content.match_indices('\n').map(|(i, _)| i + 1))
                        .collect();

                    let mut match_info = Vec::new();
                    for byte_idx in content.match_indices(&search).map(|(i, _)| i) {
                        let line_num = match line_starts.binary_search(&byte_idx) {
                            Ok(i) => i + 1,
                            Err(i) => i,
                        };
                        let ls = line_starts.get(line_num - 1).copied().unwrap_or(0);
                        let le = content[ls..]
                            .find('\n')
                            .map(|e| ls + e)
                            .unwrap_or(content.len());
                        let text: String = content[ls..le].chars().take(100).collect();
                        match_info.push(format!("  Line {}: {}", line_num, text));
                    }

                    return Err(ToolError::Msg(format!(
                        "{label}search text matched {} times in {}:\n{}\n\nAdd more surrounding context to the SEARCH block to make it unique.",
                        count,
                        path,
                        match_info.join("\n"),
                    )));
                }
                resolved.push(ResolvedSim {
                    byte_start: pos,
                    byte_end: pos + search.len(),
                    replace: replace.clone(),
                    note: String::new(),
                });
            }
            MatchResult::Normalized(start, end) => {
                resolved.push(ResolvedSim {
                    byte_start: start,
                    byte_end: end,
                    replace: replace.clone(),
                    note: format!(
                        "matched after whitespace normalization; replaced region:\n{}",
                        bounded_match_excerpt(content, start, end)
                    ),
                });
            }
            MatchResult::AmbiguousNormalized(first_line, second_line) => {
                return Err(ToolError::Msg(format!(
                    "{label}search text matched more than once after whitespace normalization in {} (including lines {} and {}).\n\nAdd more surrounding context to the SEARCH block to make it unique.",
                    path, first_line, second_line,
                )));
            }
            MatchResult::FuzzyApply(start, end, sim) => {
                resolved.push(ResolvedSim {
                    byte_start: start,
                    byte_end: end,
                    replace: replace.clone(),
                    note: format!(
                        "fuzzy match, {:.0}% similarity; replaced region:\n{}",
                        sim * 100.0,
                        bounded_match_excerpt(content, start, end)
                    ),
                });
            }
            MatchResult::AmbiguousFuzzy(first, second) => {
                return Err(ToolError::Msg(format!(
                    "{label}search text had multiple fuzzy matches above the apply threshold in {}: line {} ({:.0}%) and line {} ({:.0}%).\n\nCopy the exact text or add more surrounding context to make the SEARCH block unique.",
                    path,
                    first.0,
                    first.1 * 100.0,
                    second.0,
                    second.1 * 100.0,
                )));
            }
            MatchResult::FuzzySuggest(line, sim, preview) => {
                return Err(ToolError::Msg(format!(
                    "{label}search text not found in '{}'. Closest match at line {}, {:.0}% similar:\n  {}\n\nRead the file around that area, copy the exact text, and retry the edit.",
                    path,
                    line,
                    sim * 100.0,
                    preview,
                )));
            }
            MatchResult::NotFound => {
                return Err(ToolError::Msg(format!(
                    "{label}search text not found in '{}'.\nRead the file and copy the exact text for the SEARCH block, ensuring whitespace and indentation match.",
                    path,
                )));
            }
        }
    }

    let mut notes = Vec::new();
    let mut ranges = Vec::new();

    for rb in &resolved {
        if !rb.note.is_empty() {
            notes.push(rb.note.clone());
        }
        ranges.push((rb.byte_start, rb.byte_end, rb.replace.clone()));
    }

    Ok((notes, ranges))
}

// ── V2: Hashedit (tag-based) ────────────────────────────────────────────

fn parse_tagged_line(raw: &str) -> Option<(usize, String)> {
    let stripped = raw.trim_start_matches([' ', '\t']);
    let num_tag = stripped
        .split_once(' ')
        .map_or(stripped, |(num_tag, _content)| num_tag);
    let (num_str, tag) = num_tag.split_once('|')?;
    let line_num: usize = num_str.parse().ok()?;
    if tag.len() != 8 || !tag.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some((line_num, tag.to_string()))
}

fn extract_line_info(lines_raw: &str) -> Result<Vec<(usize, String)>, ToolError> {
    let mut result = Vec::new();
    for line in lines_raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (line_num, tag) = parse_tagged_line(line).ok_or_else(|| {
            ToolError::Msg(format!(
                "Invalid tagged line format. Expected 'N|TAG content', got: '{}'",
                trimmed
            ))
        })?;
        result.push((line_num, tag));
    }
    if result.is_empty() {
        return Err(ToolError::Msg(
            "No valid tagged lines found. Copy lines from the read output exactly.".to_string(),
        ));
    }
    Ok(result)
}

fn validate_tag(content_lines: &[&str], line_num: usize, tag: &str) -> Result<(), ToolError> {
    if line_num == 0 {
        return Err(ToolError::Msg(
            "Line 0 is invalid: tagged lines are numbered from 1. Copy the line number from the read output.".to_string(),
        ));
    }
    let idx = line_num - 1;
    let actual = content_lines.get(idx).ok_or_else(|| {
        ToolError::Msg(format!(
            "Line {} is out of range (file has {} lines)",
            line_num,
            content_lines.len()
        ))
    })?;
    let expected = crc32_hex(actual.as_bytes());
    if expected != tag {
        return Err(ToolError::Msg(format!(
            "Tag mismatch at line {}: expected {} but line content has tag {}. The file may have changed. Re-read and retry.",
            line_num, tag, expected
        )));
    }
    Ok(())
}

fn line_range_to_byte_range(
    spans: &[LineSpan<'_>],
    start_line: usize,
    end_line: usize,
    delete_whole_lines: bool,
) -> (usize, usize) {
    if spans.is_empty() || start_line == 0 || start_line > spans.len() {
        return (0, 0);
    }
    let end_line = end_line.min(spans.len());
    let start = spans[start_line - 1].start;
    let last = spans[end_line - 1];
    let end = if delete_whole_lines {
        last.full_end
    } else {
        last.content_end
    };
    (start, end)
}

async fn handle_hashedit(
    path: &str,
    file_crc: &str,
    edits: &[EditOp],
    content: &str,
) -> Result<(Vec<String>, Vec<(usize, usize, String)>), ToolError> {
    // Validate file-level CRC
    let actual_crc = crc32_hex(content.replace("\r\n", "\n").as_bytes());
    if actual_crc != file_crc {
        return Err(ToolError::Msg(format!(
            "File CRC mismatch for '{}': expected {} but file now has {}. The file has changed since the read. Re-read and retry.",
            path, file_crc, actual_crc
        )));
    }

    let spans = line_spans(content);
    let content_lines: Vec<&str> = spans.iter().map(|span| span.text).collect();
    let notes = Vec::new();
    let mut ranges = Vec::new();
    let line_ending = dominant_line_ending(content);

    for (i, op) in edits.iter().enumerate() {
        let label = if edits.len() > 1 {
            format!("Edit {}: ", i + 1)
        } else {
            String::new()
        };

        match (&op.line, &op.lines) {
            (Some(single_line), None) => {
                let (line_num, tag) = parse_tagged_line(single_line).ok_or_else(|| {
                    ToolError::Msg(format!(
                        "{}invalid tagged line format. Expected 'N|TAG content', got: '{}'",
                        label, single_line
                    ))
                })?;
                validate_tag(&content_lines, line_num, &tag)
                    .map_err(|e| ToolError::Msg(format!("{}{}", label, e)))?;

                let (byte_start, byte_end) =
                    line_range_to_byte_range(&spans, line_num, line_num, op.text.is_empty());
                ranges.push((
                    byte_start,
                    byte_end,
                    with_line_ending(&op.text, line_ending),
                ));
            }
            (None, Some(multi_lines)) => {
                let entries = extract_line_info(multi_lines)?;
                for &(line_num, ref tag) in &entries {
                    validate_tag(&content_lines, line_num, tag)
                        .map_err(|e| ToolError::Msg(format!("{}{}", label, e)))?;
                }
                // The range is `first..=last`, so the entries must describe
                // exactly that span: strictly ascending with no gaps.
                for pair in entries.windows(2) {
                    let (prev, next) = (pair[0].0, pair[1].0);
                    if next <= prev {
                        return Err(ToolError::Msg(format!(
                            "{}tagged lines must be in ascending order, but line {} follows line {}. Copy the lines from the read output in file order.",
                            label, next, prev
                        )));
                    }
                    if next != prev + 1 {
                        return Err(ToolError::Msg(format!(
                            "{}tagged lines must be contiguous, but line {} follows line {} (lines {}-{} are missing). Include every line of the range, or use separate edits.",
                            label,
                            next,
                            prev,
                            prev + 1,
                            next - 1
                        )));
                    }
                }
                let start_line = entries[0].0;
                let end_line = entries[entries.len() - 1].0;
                let (byte_start, byte_end) =
                    line_range_to_byte_range(&spans, start_line, end_line, op.text.is_empty());
                ranges.push((
                    byte_start,
                    byte_end,
                    with_line_ending(&op.text, line_ending),
                ));
            }
            (Some(_), Some(_)) => {
                return Err(ToolError::Msg(format!(
                    "{}both 'line' and 'lines' specified — use only one",
                    label
                )));
            }
            (None, None) => {
                return Err(ToolError::Msg(format!(
                    "{}neither 'line' nor 'lines' specified — provide one",
                    label
                )));
            }
        }
    }

    Ok((notes, ranges))
}

/// Reject edits whose byte ranges overlap. Ranges are applied independently
/// last-to-first, so overlapping ones would splice into each other's text and
/// garble the file; adjacent ranges (one ending where the next starts) are fine.
fn reject_overlapping_ranges(
    ranges: &[(usize, usize, String)],
    content: &str,
    label: &str,
) -> Result<(), ToolError> {
    let line_of = |byte: usize| content[..byte.min(content.len())].matches('\n').count() + 1;
    let mut ordered: Vec<(usize, usize, usize)> = ranges
        .iter()
        .enumerate()
        .map(|(i, (start, end, _))| (*start, *end, i))
        .collect();
    ordered.sort();
    for pair in ordered.windows(2) {
        let (a_start, a_end, a_idx) = pair[0];
        let (b_start, b_end, b_idx) = pair[1];
        if b_start < a_end || (a_start == b_start && a_end == b_end) {
            let (first, second) = if a_idx < b_idx {
                (pair[0], pair[1])
            } else {
                (pair[1], pair[0])
            };
            return Err(ToolError::Msg(format!(
                "{label} {} (lines {}-{}) and {label} {} (lines {}-{}) overlap. Each edit must target a distinct region; merge them into one edit.",
                first.2 + 1,
                line_of(first.0),
                line_of(first.1),
                second.2 + 1,
                line_of(second.0),
                line_of(second.1),
            )));
        }
    }
    Ok(())
}

// ── Tool implementation ──────────────────────────────────────────────────

impl Tool for EditTool {
    const NAME: &'static str = "edit";

    type Error = ToolError;
    type Args = EditArgs;
    type Output = String;

    fn description(&self) -> String {
        match edit_system() {
            EditSystem::Similarity => "Edit a file using aider-style SEARCH/REPLACE blocks. Each block finds exact text and replaces it. Multiple blocks in one call are applied atomically. If the search text is not an exact match, whitespace normalization and fuzzy matching are attempted as fallbacks.".to_string(),
            EditSystem::Hashedit => "Edit a file using tag-based line references. Copy tagged lines from read output. Edit is CAS-guarded via file-level CRC-32 hash. All edits in one call are applied atomically.".to_string(),
        }
    }

    fn parameters(&self) -> serde_json::Value {
        match edit_system() {
            EditSystem::Similarity => serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file (relative or absolute)" },
                    "block": { "type": "string", "description": "One or more SEARCH/REPLACE blocks:\n<<<<<<< SEARCH\nexisting code to find\n=======\nreplacement code\n>>>>>>> REPLACE\n\nInclude multiple blocks for separate edits to the same file." }
                },
                "required": ["path", "block"]
            }),
            EditSystem::Hashedit => serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file (relative or absolute)" },
                    "file_crc": { "type": "string", "description": "8-char hex CRC-32 from the read output header [CRC: ...]" },
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "line": { "type": "string", "description": "For single-line edits: copy-paste the tagged line from read output. Format: 'N|TAG content'" },
                                "lines": { "type": "string", "description": "For range edits: copy-paste multiple tagged lines from read output. Newline-separated." },
                                "text": { "type": "string", "description": "Replacement text. Use empty string to delete." }
                            },
                            "required": ["text"]
                        },
                        "description": "Array of edit operations"
                    }
                },
                "required": ["path", "file_crc", "edits"]
            }),
        }
    }

    async fn call(&self, args: EditArgs) -> Result<String, ToolError> {
        let workspace_root =
            crate::agent::tools::validate_workspace_binding(self.workspace.as_ref())?;
        let requested =
            crate::agent::tools::resolve_tool_path(workspace_root.as_deref(), &args.path);
        let expanded = requested.to_string_lossy().into_owned();
        let relative = std::path::Path::new(&args.path);
        let bound_workspace = if !relative.is_absolute() && !args.path.starts_with('~') {
            self.workspace.as_ref()
        } else {
            None
        };
        let capability_file = bound_workspace
            .map(|workspace| workspace.open_relative(relative))
            .transpose()?;
        let capability_metadata = capability_file
            .as_ref()
            .map(crate::fs::checked_file_metadata)
            .transpose()?;
        let resolved = if bound_workspace.is_none() {
            Some(tokio::fs::canonicalize(&requested).await?)
        } else {
            None
        };
        let path = resolved
            .as_deref()
            .unwrap_or(requested.as_path())
            .to_string_lossy()
            .into_owned();
        let approved_parent = if let Some(resolved) = &resolved {
            Some(
                crate::fs::stable_path_metadata(resolved.parent().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "edit target has no parent directory",
                    )
                })?)
                .await?,
            )
        } else {
            None
        };
        let es = edit_system();
        tracing::debug!(
            "tool edit start: path={}, mode={:?}, has_block={}, has_edits={}",
            expanded,
            es,
            args.block.is_some(),
            args.edits.as_ref().map(|e| e.len()).unwrap_or(0),
        );
        // Check the path atomic_write will modify, not a symlink that points to it.
        let coaching = if let Some(workspace) = bound_workspace {
            check_perm_bound_path(&self.permission, &self.ask_tx, "edit", workspace, relative)
                .await?
        } else {
            check_perm_path(&self.permission, &self.ask_tx, "edit", &path).await?
        };

        let mut file = if let Some(file) = capability_file {
            tokio::fs::File::from_std(file)
        } else {
            crate::fs::open_stable_file(
                resolved
                    .as_deref()
                    .expect("ambient edit must resolve an external path"),
            )
            .await?
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).await?;
        // Fail closed on invalid UTF-8: the whole file is rewritten, so a lossy
        // decode would replace every invalid byte (even in untouched regions)
        // with U+FFFD and silently corrupt e.g. Latin-1 files.
        let content = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(e) => {
                return Err(ToolError::Msg(format!(
                    "Cannot edit '{}': file is not valid UTF-8 (invalid byte at offset {}). The edit tool only handles UTF-8 text; convert the file first or use bash for binary/legacy encodings.",
                    expanded,
                    e.utf8_error().valid_up_to(),
                )));
            }
        };

        // Determine mode: V1 (block) or V2 (edits)
        let (notes, mut ranges) = if let Some(ref block) = args.block {
            handle_similarity(&path, block, &content).await?
        } else if let (Some(file_crc), Some(edits)) = (&args.file_crc, &args.edits) {
            handle_hashedit(&path, file_crc, edits, &content).await?
        } else if args.block.is_some() {
            // block was Some but empty or parse failed — handle_similarity already errored
            unreachable!()
        } else {
            return Err(ToolError::Msg(
                "Provide either 'block' (SEARCH/REPLACE) or 'file_crc'+'edits' (hashedit). Use /editsys to check the current mode."
                    .to_string(),
            ));
        };

        let edit_count = ranges.len();

        let edit_label = if args.block.is_some() {
            "Block"
        } else {
            "Edit"
        };
        reject_overlapping_ranges(&ranges, &content, edit_label)?;

        // Apply last-to-first so earlier byte positions remain valid
        ranges.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));

        let mut modified = content;

        for (byte_start, byte_end, replace) in &ranges {
            if *byte_end > modified.len() || *byte_start > modified.len() {
                return Err(ToolError::Msg(
                    "Internal error: edit range exceeds file bounds. The file may have changed. Re-read and retry."
                        .to_string(),
                ));
            }
            modified.replace_range(*byte_start..*byte_end, replace);
        }

        let output = modified;

        if let (Some(workspace), Some(expected)) = (&self.workspace, &capability_metadata) {
            workspace.replace_relative_atomic(relative, output.as_bytes(), expected)?;
        } else {
            let resolved = resolved.expect("external edit must resolve an ambient path");
            let current = tokio::fs::canonicalize(&requested).await?;
            if current != resolved {
                return Err(ToolError::Msg(format!(
                    "Path changed after permission check: {}",
                    expanded
                )));
            }
            crate::fs::atomic_write_resolved_checked(
                &resolved,
                &output,
                approved_parent.expect("external edit must capture its parent"),
            )
            .await?;
        }
        self.read_tracker.untrack_read_path(&path);

        tracing::debug!(
            "tool edit done: path={}, edit_count={}, notes={}",
            expanded,
            edit_count,
            notes.len(),
        );
        let mut result = format!("Applied {} edit(s) to {}", edit_count, expanded);
        for note in &notes {
            result.push_str(&format!("\n  Note: {}", note));
        }
        if let Some(msg) = coaching {
            result = format!("{}\n\n{}", msg, result);
        }

        #[cfg(feature = "lsp")]
        if let Some(lsp) = &self.lsp {
            let file = std::path::Path::new(&path);
            if capability_metadata.is_some() {
                lsp.notify_changed_relative(relative).await;
            } else {
                lsp.notify_changed(file).await;
            }
            let diagnostics = if capability_metadata.is_some() {
                lsp.diagnostics_block_for_relative_edit(relative).await
            } else {
                lsp.diagnostics_block_for_edit(file).await
            };
            if let Some(block) = diagnostics {
                result.push_str(&block);
            }
        }

        Ok(result)
    }
}

#[cfg(test)]
mod match_excerpt_tests {
    use super::bounded_match_excerpt;

    #[test]
    fn non_exact_match_excerpt_is_bounded_on_character_boundaries() {
        let content = "é".repeat(300);
        let excerpt = bounded_match_excerpt(&content, 0, content.len());

        assert_eq!(excerpt.chars().count(), 241);
        assert!(excerpt.ends_with('…'));
        assert_eq!(excerpt.trim_end_matches('…'), "é".repeat(240));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::permission::checker::PermissionChecker;
    use crate::permission::{PermissionConfigs, SecurityMode};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "zerostack_edit_permission_test_{}_{}",
                std::process::id(),
                n
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn checks_permission_on_symlink_target_before_edit() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new();
        let allowed_dir = temp.path().join("allowed");
        let restricted_dir = temp.path().join("restricted");
        std::fs::create_dir_all(&allowed_dir).unwrap();
        std::fs::create_dir_all(&restricted_dir).unwrap();

        let restricted_target = restricted_dir.join("existing.txt");
        let original = "original contents\n";
        std::fs::write(&restricted_target, original).unwrap();
        let allowed_link = allowed_dir.join("safe-link.txt");
        symlink(&restricted_target, &allowed_link).unwrap();

        let checker = PermissionChecker::new(
            &PermissionConfigs::default(),
            SecurityMode::Standard,
            Some(allowed_dir),
            Some(vec!["standard".to_string()]),
        )
        .expect("valid permission test configuration");
        let tool = EditTool::new(Some(Arc::new(Mutex::new(checker))), None);

        let error = tool
            .call(EditArgs {
                path: allowed_link.to_string_lossy().into_owned(),
                block: Some(
                    "<<<<<<< SEARCH\noriginal contents\n=======\nmodified contents\n>>>>>>> REPLACE"
                        .to_string(),
                ),
                file_crc: None,
                edits: None,
            })
            .await
            .expect_err("the resolved external target must require permission");

        assert!(
            error.to_string().contains("Permission denied"),
            "unexpected error: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(&restricted_target).unwrap(),
            original,
            "permission denial must happen before the symlink target is edited"
        );
    }

    #[tokio::test]
    async fn symlink_swap_after_permission_check_is_rejected() {
        use std::os::unix::fs::symlink;

        use crate::permission::ask::UserDecision;

        let temp = TempDir::new();
        let checked_target = temp.path().join("checked.txt");
        let swapped_target = temp.path().join("swapped.txt");
        let link = temp.path().join("input.txt");
        std::fs::write(&checked_target, "original checked contents\n").unwrap();
        std::fs::write(&swapped_target, "original swapped contents\n").unwrap();
        symlink(&checked_target, &link).unwrap();

        let checker = PermissionChecker::new(
            &PermissionConfigs::default(),
            SecurityMode::Guarded,
            Some(temp.path().to_path_buf()),
            Some(vec!["guarded".to_string()]),
        )
        .expect("valid permission test configuration");
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let tool = EditTool::new(Some(Arc::new(Mutex::new(checker))), Some(ask_tx));

        let call = tool.call(EditArgs {
            path: link.to_string_lossy().into_owned(),
            block: Some(
                "<<<<<<< SEARCH\noriginal checked contents\n=======\nmodified checked contents\n>>>>>>> REPLACE"
                    .to_string(),
            ),
            file_crc: None,
            edits: None,
        });
        let swap = async {
            let request = ask_rx.recv().await.expect("permission request");
            assert_eq!(
                PathBuf::from(&request.input),
                std::fs::canonicalize(&checked_target).unwrap()
            );
            std::fs::remove_file(&checked_target).unwrap();
            symlink(&swapped_target, &checked_target).unwrap();
            request.reply.send(UserDecision::AllowOnce).unwrap();
        };

        let (result, ()) = tokio::join!(call, swap);
        let error = result.expect_err("edit must reject a swapped permission-checked target");
        assert!(error.to_string().contains("Path changed"));
        assert_eq!(
            std::fs::read_to_string(&swapped_target).unwrap(),
            "original swapped contents\n"
        );
    }
}
