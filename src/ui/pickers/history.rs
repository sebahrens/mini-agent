//! Ctrl+R reverse history search over the prompt history (global chat
//! history plus this session's submissions).
//!
//! While open, the input buffer holds the search query; the list shows
//! matching entries newest first (exact, then prefix, then substring
//! matches, ties newest first). Enter or Tab inserts the highlighted entry
//! for editing (it is never submitted), Esc restores the draft, and pressing
//! the search key again moves to the next older match.

use std::collections::HashMap;

use compact_str::CompactString;
use crossterm::event::{KeyCode, KeyEvent};

use super::handlers::handle_prefixed_key;
use super::list::ListPicker;

/// Distinct entries offered (newest first).
const MAX_HISTORY_SEARCH_ENTRIES: usize = 10_000;
/// Characters of a single-line preview.
const PREVIEW_CHARS: usize = 200;

/// State kept while the search is open.
#[derive(Debug, Default)]
pub(crate) struct HistorySearch {
    /// The draft (and caret) replaced by the query, restored on Esc.
    pub(crate) draft: CompactString,
    pub(crate) cursor: usize,
    /// Preview row to the full (possibly multi-line) entry.
    pub(crate) full: HashMap<String, String>,
}

/// One-line display of `entry`: newlines collapse to ` ⏎ ` and long entries
/// are cut with an ellipsis.
pub(crate) fn preview(entry: &str) -> String {
    let flattened = entry
        .trim()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ⏎ ");
    let mut chars = flattened.chars();
    let head: String = chars.by_ref().take(PREVIEW_CHARS).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Distinct previews of `history` (oldest first), newest first, with the
/// full entry each one stands for.
pub(crate) fn history_items<S: AsRef<str>>(
    history: &[S],
) -> (Vec<String>, HashMap<String, String>) {
    let mut items = Vec::new();
    let mut full = HashMap::new();
    for entry in history.iter().rev() {
        let text = entry.as_ref().trim();
        if text.is_empty() {
            continue;
        }
        let row = preview(text);
        if full.contains_key(&row) {
            continue;
        }
        full.insert(row.clone(), text.to_owned());
        items.push(row);
        if items.len() >= MAX_HISTORY_SEARCH_ENTRIES {
            break;
        }
    }
    (items, full)
}

/// Keys while the search is open. Returns whether the key was consumed.
pub(crate) fn handle_history_key(
    buffer: &mut CompactString,
    cursor: &mut usize,
    picker: &mut ListPicker,
    search: &mut HistorySearch,
    key: KeyEvent,
) -> bool {
    match key.code {
        KeyCode::Esc => {
            *buffer = std::mem::take(&mut search.draft);
            *cursor = search.cursor.min(buffer.len());
            picker.deactivate();
            true
        }
        KeyCode::Enter | KeyCode::Tab => {
            if let Some(selected) = picker.selected_name() {
                let entry = search
                    .full
                    .get(selected)
                    .cloned()
                    .unwrap_or_else(|| selected.to_owned());
                *buffer = entry.into();
                *cursor = buffer.len();
            }
            picker.deactivate();
            true
        }
        _ => handle_prefixed_key(buffer, cursor, picker, "", key),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn items_are_newest_first_deduplicated_and_map_back_to_full_entries() {
        let history = ["fix the build", "multi\nline prompt", "fix the build", "  "];
        let (items, full) = history_items(&history);
        assert_eq!(items, vec!["fix the build", "multi ⏎ line prompt"]);
        assert_eq!(full["multi ⏎ line prompt"], "multi\nline prompt");
        assert!(preview(&"x".repeat(500)).ends_with('…'));
    }

    #[test]
    fn typing_filters_enter_inserts_and_esc_restores_the_draft() {
        let history = [
            "deploy staging",
            "run cargo test",
            "cargo fmt",
            "multi\ncargo clippy",
        ];
        let (items, full) = history_items(&history);
        let mut picker = ListPicker::new();
        picker.set_items(items);
        picker.activate();
        let mut search = HistorySearch {
            draft: "half-typed".into(),
            cursor: 4,
            full,
        };
        let mut buffer = CompactString::default();
        let mut cursor = 0;
        for c in "cargo".chars() {
            assert!(handle_history_key(
                &mut buffer,
                &mut cursor,
                &mut picker,
                &mut search,
                key(KeyCode::Char(c))
            ));
        }
        assert_eq!(buffer, "cargo");
        let matches: Vec<&str> = picker.matches().collect();
        assert!(matches.contains(&"cargo fmt") && !matches.contains(&"deploy staging"));
        assert_eq!(matches[0], "cargo fmt", "prefix match first");
        assert!(handle_history_key(
            &mut buffer,
            &mut cursor,
            &mut picker,
            &mut search,
            key(KeyCode::Enter)
        ));
        assert_eq!(buffer, "cargo fmt");
        assert!(!picker.active);

        // Esc restores the draft that the query replaced.
        picker.activate();
        buffer = "query".into();
        assert!(handle_history_key(
            &mut buffer,
            &mut cursor,
            &mut picker,
            &mut search,
            key(KeyCode::Esc)
        ));
        assert_eq!(buffer, "half-typed");
        assert_eq!(cursor, 4);
    }
}
