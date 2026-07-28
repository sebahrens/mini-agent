use std::path::{Path, PathBuf};

use compact_str::CompactString;
use serde::{Deserialize, Serialize};

use crate::session::storage;

const MAX_CHAT_HISTORY_ENTRIES: usize = 10_000;

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

fn append_entry_to_path(
    path: &Path,
    entry: &ChatHistoryEntry,
    max_entries: usize,
) -> anyhow::Result<()> {
    let mut entries: Vec<ChatHistoryEntry> = if path.exists() {
        let content = std::fs::read_to_string(&path)?;
        match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => {
                // File is corrupt — back it up rather than silently discarding
                // all prior history.
                let bak = path.with_extension("json.bak");
                let _ = std::fs::rename(&path, &bak);
                tracing::warn!("chat history was corrupt, backed up to {:?}", bak);
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    entries.push(entry.clone());
    let excess = entries.len().saturating_sub(max_entries);
    if excess > 0 {
        drop(entries.drain(..excess));
    }
    let json = serde_json::to_string_pretty(&entries)?;
    storage::atomic_write(path, &json)?;
    Ok(())
}

pub fn load_history() -> anyhow::Result<Vec<ChatHistoryEntry>> {
    if crate::paths::artifact_disabled("chat history") {
        return Ok(Vec::new());
    }
    let path = chat_history_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&content).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{ChatHistoryEntry, append_entry_to_path};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "mini-agent-chat-history-{}",
                uuid::Uuid::new_v4()
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

        let file_size = std::fs::metadata(&path).unwrap().len();
        let entries: Vec<ChatHistoryEntry> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();

        assert_eq!(entries.len(), TEST_LIMIT);
        assert_eq!(entries.first().unwrap().content, "entry-0900");
        assert_eq!(entries.last().unwrap().content, "entry-0999");
        assert!(file_size < 10_000, "history file grew to {file_size} bytes");
    }
}
