use std::path::{Path, PathBuf};

use compact_str::CompactString;
use serde::{Deserialize, Serialize};

use crate::session::storage;

pub(crate) const MAX_CHAT_HISTORY_ENTRIES: usize = 10_000;
/// Compact only when line count exceeds 2x the maximum (lazy compaction).
const COMPACTION_THRESHOLD_MULTIPLIER: usize = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatHistoryEntry {
    pub content: String,
    pub timestamp: CompactString,
}

fn chat_history_path() -> PathBuf {
    crate::paths::process_paths()
        .expect("startup must initialize application paths")
        .chat_history_file()
}

pub fn append_entry(entry: &ChatHistoryEntry) -> anyhow::Result<()> {
    if crate::paths::artifact_disabled("chat history") {
        return Ok(());
    }
    let path = chat_history_path();
    append_entry_to_path(&path, entry, MAX_CHAT_HISTORY_ENTRIES)
}

/// Append one entry to chat history using JSONL format (O(1) append operation).
/// Handles migration from legacy JSON array format.
/// Lazily compacts when actual line count exceeds 2x the maximum.
fn append_entry_to_path(
    path: &Path,
    entry: &ChatHistoryEntry,
    max_entries: usize,
) -> anyhow::Result<()> {
    // Ensure parent directory exists with private permissions.
    if let Some(parent) = path.parent() {
        crate::paths::ensure_private_directory(parent)?;
    }

    // Check if file exists and is in legacy JSON array format.
    // If so, migrate to JSONL before appending.
    if path.exists() {
        let content = std::fs::read_to_string(path)?;
        if is_legacy_json_format(&content) {
            // Parse legacy array and migrate to JSONL.
            migrate_legacy_to_jsonl(path, &content, entry, max_entries)?;
            return Ok(());
        }
    }

    // Serialize the new entry as JSONL (one per line).
    let json_line = format!("{}\n", serde_json::to_string(entry)?);

    // Try to append to existing file, or create if missing.
    match append_line_to_file(path, &json_line) {
        Ok(()) => {
            // Check if compaction is needed (lazy compaction at 2x threshold).
            check_and_compact_if_needed(path, max_entries)?;
            Ok(())
        }
        Err(_e) => {
            // File exists but is corrupt/unreadable. Back it up and start fresh.
            if path.exists() {
                let bak = path.with_extension("json.bak");
                if let Err(rename_err) = std::fs::rename(path, &bak) {
                    tracing::warn!("failed to back up corrupt chat history: {}", rename_err);
                } else {
                    tracing::warn!("chat history was corrupt, backed up to {:?}", bak);
                }
            }
            // Create a fresh file with just the new entry.
            storage::atomic_write(path, &json_line)?;
            Ok(())
        }
    }
}

/// Migrate a legacy JSON array file to JSONL format and append a new entry.
fn migrate_legacy_to_jsonl(
    path: &Path,
    content: &str,
    new_entry: &ChatHistoryEntry,
    max_entries: usize,
) -> anyhow::Result<()> {
    // Parse the legacy JSON array.
    let mut entries: Vec<ChatHistoryEntry> = serde_json::from_str(content).unwrap_or_default();

    // Add the new entry.
    entries.push(new_entry.clone());

    // Enforce the cap.
    let excess = entries.len().saturating_sub(max_entries);
    if excess > 0 {
        entries.drain(..excess);
    }

    // Rebuild as JSONL and write atomically.
    let mut jsonl = String::new();
    for entry in entries {
        jsonl.push_str(&serde_json::to_string(&entry)?);
        jsonl.push('\n');
    }

    storage::atomic_write(path, &jsonl)?;
    Ok(())
}

/// Append a single line to a file, creating it with private permissions if needed.
/// Returns error if the file exists but cannot be appended to.
fn append_line_to_file(path: &Path, line: &str) -> anyhow::Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write;

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(line.as_bytes())?;
        file.flush()?;
    }

    #[cfg(windows)]
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .append(true)
            .open(path)?;
        file.write_all(line.as_bytes())?;
        file.flush()?;
    }

    Ok(())
}

/// Count actual newlines in the file to determine line count.
/// This is the true measure of entries, regardless of entry size.
fn count_lines(path: &Path) -> anyhow::Result<usize> {
    let content = std::fs::read_to_string(path)?;
    if content.is_empty() {
        return Ok(0);
    }
    // Count lines: each line ends with '\n', so count '\n' occurrences.
    let line_count = content.matches('\n').count();
    Ok(line_count)
}

/// Check if compaction is needed and perform it if necessary.
/// Uses a cheap pre-filter (file size heuristic) to avoid reading the entire file,
/// then counts actual lines only if the pre-filter suggests we might be over the threshold.
fn check_and_compact_if_needed(path: &Path, max_entries: usize) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }

    // Pre-filter: if file is obviously small, skip. Use a conservative estimate.
    const MIN_BYTES_PER_ENTRY: u64 = 40; // Minimum JSON overhead + tiny content
    let prefilter_threshold =
        (max_entries * COMPACTION_THRESHOLD_MULTIPLIER) as u64 * MIN_BYTES_PER_ENTRY;
    let file_size = std::fs::metadata(path)?.len();

    if file_size < prefilter_threshold {
        // File is definitely under the threshold; no need to count lines.
        return Ok(());
    }

    // Pre-filter triggered: count actual lines to make a real decision.
    let line_count = count_lines(path)?;
    let compaction_threshold = max_entries * COMPACTION_THRESHOLD_MULTIPLIER;

    if line_count > compaction_threshold {
        compact_history(path, max_entries)?;
    }

    Ok(())
}

/// Load all entries from JSONL file, skipping corrupt lines.
fn load_entries_from_jsonl(path: &Path) -> anyhow::Result<Vec<ChatHistoryEntry>> {
    let content = std::fs::read_to_string(path)?;
    let mut entries = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Skip lines that fail to parse; don't error on corruption.
        if let Ok(entry) = serde_json::from_str::<ChatHistoryEntry>(trimmed) {
            entries.push(entry);
        }
    }

    Ok(entries)
}

/// Compact the history file by rewriting it with only the most recent entries.
fn compact_history(path: &Path, max_entries: usize) -> anyhow::Result<()> {
    let entries = load_entries_from_jsonl(path)?;

    // Keep only the most recent max_entries.
    let to_keep = if entries.len() > max_entries {
        entries.len() - max_entries
    } else {
        0
    };
    let trimmed: Vec<_> = entries.into_iter().skip(to_keep).collect();

    // Rebuild the JSONL file with selected entries.
    let mut content = String::new();
    for entry in trimmed {
        content.push_str(&serde_json::to_string(&entry)?);
        content.push('\n');
    }

    storage::atomic_write(path, &content)?;
    Ok(())
}

/// Load history from a specific path with an explicit cap.
/// Auto-detects legacy JSON array format and loads it.
/// Enforces the cap at read time: returned entries are at most max_entries.
fn load_history_from_path(
    path: &Path,
    max_entries: usize,
) -> anyhow::Result<Vec<ChatHistoryEntry>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(path)?;

    let mut entries = if is_legacy_json_format(&content) {
        // Legacy JSON array format: parse directly.
        serde_json::from_str::<Vec<ChatHistoryEntry>>(&content).unwrap_or_default()
    } else {
        // JSONL format: load and skip corrupt lines.
        load_entries_from_jsonl(path).unwrap_or_default()
    };

    // Enforce the cap at read time. This ensures the observable behavior (what
    // callers see) is always bounded, even though the file may temporarily hold
    // more entries due to lazy compaction.
    let excess = entries.len().saturating_sub(max_entries);
    if excess > 0 {
        entries.drain(..excess);
    }

    Ok(entries)
}

pub fn load_history() -> anyhow::Result<Vec<ChatHistoryEntry>> {
    if crate::paths::artifact_disabled("chat history") {
        return Ok(Vec::new());
    }
    let path = chat_history_path();
    load_history_from_path(&path, MAX_CHAT_HISTORY_ENTRIES)
}

/// Detect if content is legacy JSON array (starts with '[') vs JSONL.
fn is_legacy_json_format(content: &str) -> bool {
    let trimmed = content.trim();
    trimmed.starts_with('[')
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{ChatHistoryEntry, append_entry_to_path, load_history_from_path};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            // macOS exposes its temp directory through `/var`, which is a symlink
            // to `/private/var`; persistence intentionally rejects linked parents.
            let temp_root = std::env::temp_dir()
                .canonicalize()
                .expect("failed to resolve the chat-history test temp root");
            let path = temp_root.join(format!("mini-agent-chat-history-{}", uuid::Uuid::new_v4()));
            crate::paths::ensure_private_directory(&path).unwrap();
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

    #[test]
    fn appending_1000_entries_keeps_history_bounded() {
        const TEST_LIMIT: usize = 100;

        let temp = TempDir::new();
        let path = temp.path().join("chat_history.json");

        for index in 0..1_000 {
            append_entry_to_path(
                &path,
                &ChatHistoryEntry {
                    content: format!("entry-{index:04}"),
                    timestamp: "2026-07-29T00:00:00Z".into(),
                },
                TEST_LIMIT,
            )
            .unwrap();
        }

        // Load via production code with explicit cap that matches the append cap.
        let entries = load_history_from_path(&path, TEST_LIMIT).unwrap();

        // Production code enforces the cap at read time.
        assert_eq!(
            entries.len(),
            TEST_LIMIT,
            "load_history_from_path should enforce the cap"
        );
        assert_eq!(entries.first().unwrap().content, "entry-0900");
        assert_eq!(entries.last().unwrap().content, "entry-0999");

        // Lazy compaction deliberately lets the file reach the compaction
        // threshold (2x the cap) before rewriting, so the on-disk file holds up
        // to `2 * TEST_LIMIT` entries even though reads are capped at
        // TEST_LIMIT. At ~58 bytes per serialized entry that is ~11.6 KB; allow
        // headroom for timestamp/content width without letting an unbounded
        // regression through.
        const MAX_ENTRY_BYTES: u64 = 80;
        let compaction_bound = 2 * TEST_LIMIT as u64 * MAX_ENTRY_BYTES;
        let file_size = std::fs::metadata(&path).unwrap().len();
        assert!(
            file_size < compaction_bound,
            "history file grew to {file_size} bytes, above the {compaction_bound}-byte compaction bound"
        );
    }

    #[test]
    fn legacy_json_array_migration_preserves_all_entries() {
        let temp = TempDir::new();
        let path = temp.path().join("chat_history.json");

        // Create a legacy JSON array file.
        let legacy_entries = vec![
            ChatHistoryEntry {
                content: "legacy-entry-0".to_string(),
                timestamp: "2026-07-29T00:00:00Z".into(),
            },
            ChatHistoryEntry {
                content: "legacy-entry-1".to_string(),
                timestamp: "2026-07-29T00:00:01Z".into(),
            },
            ChatHistoryEntry {
                content: "legacy-entry-2".to_string(),
                timestamp: "2026-07-29T00:00:02Z".into(),
            },
        ];
        let legacy_json = serde_json::to_string_pretty(&legacy_entries).unwrap();
        std::fs::write(&path, &legacy_json).unwrap();

        // First append triggers migration from legacy array to JSONL.
        append_entry_to_path(
            &path,
            &ChatHistoryEntry {
                content: "new-entry-3".to_string(),
                timestamp: "2026-07-29T00:00:03Z".into(),
            },
            100,
        )
        .unwrap();

        // Load via production code; all 4 entries should be present.
        let entries = load_history_from_path(&path, 100).unwrap();
        assert_eq!(
            entries.len(),
            4,
            "migration should preserve all legacy entries plus new one"
        );
        assert_eq!(entries[0].content, "legacy-entry-0");
        assert_eq!(entries[1].content, "legacy-entry-1");
        assert_eq!(entries[2].content, "legacy-entry-2");
        assert_eq!(entries[3].content, "new-entry-3");

        // File should now be in JSONL format (no leading '[').
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            !content.trim().starts_with('['),
            "file should be converted to JSONL format"
        );
    }

    #[test]
    fn corrupt_jsonl_lines_are_skipped() {
        let temp = TempDir::new();
        let path = temp.path().join("chat_history.json");

        // Create a JSONL file with some corrupted lines.
        let jsonl = r#"{"content":"good-entry-0","timestamp":"2026-07-29T00:00:00Z"}
{invalid json here}
{"content":"good-entry-1","timestamp":"2026-07-29T00:00:01Z"}

{"content":"good-entry-2","timestamp":"2026-07-29T00:00:02Z"}
"#;
        std::fs::write(&path, jsonl).unwrap();

        // Load via production code; corrupt lines should be skipped.
        let entries = load_history_from_path(&path, 100).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].content, "good-entry-0");
        assert_eq!(entries[1].content, "good-entry-1");
        assert_eq!(entries[2].content, "good-entry-2");
    }

    #[test]
    fn jsonl_appends_are_incremental() {
        const TEST_LIMIT: usize = 50;

        let temp = TempDir::new();
        let path = temp.path().join("chat_history.json");

        // Append 10 entries and check file size.
        for index in 0..10 {
            append_entry_to_path(
                &path,
                &ChatHistoryEntry {
                    content: format!("entry-{index:02}"),
                    timestamp: "2026-07-29T00:00:00Z".into(),
                },
                TEST_LIMIT,
            )
            .unwrap();
        }

        let size_after_10 = std::fs::metadata(&path).unwrap().len();

        // Append 10 more entries.
        for index in 10..20 {
            append_entry_to_path(
                &path,
                &ChatHistoryEntry {
                    content: format!("entry-{index:02}"),
                    timestamp: "2026-07-29T00:00:00Z".into(),
                },
                TEST_LIMIT,
            )
            .unwrap();
        }

        let size_after_20 = std::fs::metadata(&path).unwrap().len();

        // File should grow incrementally. Each entry serializes to ~58 bytes.
        // We expect roughly 10 * 58 = 580 bytes added for 10 entries.
        let size_diff = size_after_20 - size_after_10;
        assert!(
            size_diff > 500 && size_diff < 700,
            "10 entries at ~58 bytes each should add ~580 bytes; got {size_diff}"
        );
    }
}
