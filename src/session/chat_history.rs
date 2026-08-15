use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use compact_str::CompactString;
use serde::{Deserialize, Serialize};

use crate::session::storage;

pub(crate) const MAX_CHAT_HISTORY_ENTRIES: usize = 10_000;
/// Compact only when line count exceeds 2x the maximum (lazy compaction).
const COMPACTION_THRESHOLD_MULTIPLIER: usize = 2;

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

#[derive(Default)]
struct HistoryFileState {
    entry_count: usize,
    observed: Option<FileStamp>,
    #[cfg(test)]
    full_scans: usize,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum HistoryFormat {
    JsonLines,
    LegacyArray,
}

struct ParsedHistory {
    entries: Vec<ChatHistoryEntry>,
    format: HistoryFormat,
    corrupt: bool,
    original: String,
}

static HISTORY_FILE_STATES: OnceLock<Mutex<HashMap<PathBuf, HistoryFileState>>> = OnceLock::new();

fn history_file_states() -> &'static Mutex<HashMap<PathBuf, HistoryFileState>> {
    HISTORY_FILE_STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

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

/// Append one JSONL record. Existing content is scanned once per process (or
/// after an external file change), then an in-memory count makes ordinary
/// appends O(1). Compaction is one amortized rewrite per `max_entries` appends.
fn append_entry_to_path(
    path: &Path,
    entry: &ChatHistoryEntry,
    max_entries: usize,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        crate::paths::ensure_private_directory(parent)?;
    }

    let mut states = history_file_states()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let state = states.entry(path.to_path_buf()).or_default();
    let current = file_stamp(path)?;
    if state.observed != current {
        if current.is_some() {
            let parsed = read_history_document(path)?;
            #[cfg(test)]
            {
                state.full_scans += 1;
            }
            state.entry_count = parsed.entries.len();
            if parsed.format == HistoryFormat::LegacyArray || parsed.corrupt {
                if parsed.corrupt {
                    backup_corrupt_history(path, &parsed.original)?;
                }
                let mut entries = parsed.entries;
                trim_to_recent(&mut entries, max_entries);
                storage::atomic_write(path, &serialize_jsonl(&entries)?)?;
                state.entry_count = entries.len();
            }
        } else {
            state.entry_count = 0;
        }
        state.observed = file_stamp(path)?;
    }

    let json_line = format!("{}\n", serde_json::to_string(entry)?);
    append_line_to_file(path, &json_line)?;
    state.entry_count = state.entry_count.saturating_add(1);
    state.observed = file_stamp(path)?;

    let compaction_threshold = max_entries.saturating_mul(COMPACTION_THRESHOLD_MULTIPLIER);
    if state.entry_count > compaction_threshold {
        let parsed = read_history_document(path)?;
        #[cfg(test)]
        {
            state.full_scans += 1;
        }
        let mut entries = parsed.entries;
        trim_to_recent(&mut entries, max_entries);
        storage::atomic_write(path, &serialize_jsonl(&entries)?)?;
        state.entry_count = entries.len();
        state.observed = file_stamp(path)?;
    }

    Ok(())
}

fn file_stamp(path: &Path) -> anyhow::Result<Option<FileStamp>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(FileStamp {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn append_line_to_file(path: &Path, line: &str) -> anyhow::Result<()> {
    use std::fs::OpenOptions;

    if !path.exists() {
        crate::fs::private_atomic_create_sync(path, line.as_bytes())?;
        return Ok(());
    }

    // Reject links, foreign ownership, and non-regular files before reopening
    // for append. The containing directory is private, so another user cannot
    // swap the validated entry between these operations.
    drop(crate::fs::open_private_file(path)?);

    let mut options = OpenOptions::new();
    options.write(true).append(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }

    let mut file = options.open(path)?;
    file.write_all(line.as_bytes())?;
    file.flush()?;
    Ok(())
}

fn read_history_document(path: &Path) -> anyhow::Result<ParsedHistory> {
    let mut file = crate::fs::open_private_file(path)?;
    let mut original = String::new();
    file.read_to_string(&mut original)?;
    Ok(parse_history_document(original))
}

fn parse_history_document(original: String) -> ParsedHistory {
    if is_legacy_json_format(&original) {
        return match serde_json::from_str::<Vec<ChatHistoryEntry>>(&original) {
            Ok(entries) => ParsedHistory {
                entries,
                format: HistoryFormat::LegacyArray,
                corrupt: false,
                original,
            },
            Err(_) => ParsedHistory {
                entries: Vec::new(),
                format: HistoryFormat::LegacyArray,
                corrupt: true,
                original,
            },
        };
    }

    let mut entries = Vec::new();
    let mut corrupt = false;
    for line in original.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<ChatHistoryEntry>(trimmed) {
            Ok(entry) => entries.push(entry),
            Err(_) => corrupt = true,
        }
    }
    ParsedHistory {
        entries,
        format: HistoryFormat::JsonLines,
        corrupt,
        original,
    }
}

fn serialize_jsonl(entries: &[ChatHistoryEntry]) -> anyhow::Result<String> {
    let mut content = String::new();
    for entry in entries {
        content.push_str(&serde_json::to_string(entry)?);
        content.push('\n');
    }
    Ok(content)
}

fn trim_to_recent(entries: &mut Vec<ChatHistoryEntry>, max_entries: usize) {
    let excess = entries.len().saturating_sub(max_entries);
    if excess > 0 {
        entries.drain(..excess);
    }
}

fn backup_corrupt_history(path: &Path, original: &str) -> anyhow::Result<()> {
    let backup = path.with_extension("json.bak");
    storage::atomic_write(&backup, original)?;
    tracing::warn!("chat history was corrupt, backed up to {:?}", backup);
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

    let mut states = history_file_states()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let state = states.entry(path.to_path_buf()).or_default();
    let parsed = read_history_document(path)?;
    #[cfg(test)]
    {
        state.full_scans += 1;
    }
    let mut entries = parsed.entries;
    trim_to_recent(&mut entries, max_entries);
    if parsed.format == HistoryFormat::LegacyArray || parsed.corrupt {
        if parsed.corrupt {
            backup_corrupt_history(path, &parsed.original)?;
        }
        storage::atomic_write(path, &serialize_jsonl(&entries)?)?;
    }
    state.entry_count = entries.len();
    state.observed = file_stamp(path)?;
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

        // Loading performs the one-time migration before any new append.
        let migrated = load_history_from_path(&path, 100).unwrap();
        assert_eq!(migrated.len(), 3);
        let migrated_content = std::fs::read_to_string(&path).unwrap();
        assert!(!migrated_content.trim().starts_with('['));

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
        assert_eq!(
            std::fs::read_to_string(path.with_extension("json.bak")).unwrap(),
            jsonl,
            "the original corrupt file must remain recoverable"
        );
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("invalid json"),
            "the active file should be repaired after backup"
        );
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

        let states = super::history_file_states()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(
            states.get(&path).unwrap().full_scans,
            0,
            "a newly created, unchanged JSONL file must not be rescanned per append"
        );
    }

    #[test]
    fn external_jsonl_change_is_rescanned_once_then_returns_to_constant_time_appends() {
        let temp = TempDir::new();
        let path = temp.path().join("chat_history.json");
        let first = ChatHistoryEntry {
            content: "external-entry".into(),
            timestamp: "2026-07-29T00:00:00Z".into(),
        };
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&first).unwrap()),
        )
        .unwrap();

        for index in 0..20 {
            append_entry_to_path(
                &path,
                &ChatHistoryEntry {
                    content: format!("appended-{index}"),
                    timestamp: "2026-07-29T00:00:01Z".into(),
                },
                100,
            )
            .unwrap();
        }

        let states = super::history_file_states()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(states.get(&path).unwrap().full_scans, 1);
        drop(states);
        let entries = load_history_from_path(&path, 100).unwrap();
        assert_eq!(entries.len(), 21);
        assert_eq!(entries.first().unwrap().content, "external-entry");
    }
}
