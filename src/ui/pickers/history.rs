//! Ctrl+R reverse history search over the prompt history (global chat
//! history plus this session's submissions).
//!
//! While open, the input buffer holds the search query; the list shows
//! matching entries newest first (exact, then prefix, then substring
//! matches, ties newest first). Enter or Tab inserts the highlighted entry
//! for editing (it is never submitted), Esc restores the draft, and pressing
//! the search key again moves to the next older match.

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
    /// The full (possibly multi-line) entry behind each picker item, by
    /// item index.
    pub(crate) full: Vec<String>,
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

/// Longest one-line text of an entry that the query is matched against.
const MAX_HAYSTACK_BYTES: usize = 16 * 1024;

/// One-line, bounded text of `entry` used for matching (original case).
fn haystack(entry: &str) -> String {
    let mut text = entry
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ⏎ ");
    if text.len() > MAX_HAYSTACK_BYTES {
        let mut cut = MAX_HAYSTACK_BYTES;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    text
}

/// The distinct entries of `history` (oldest first), newest first, as
/// `(previews, full entries, match text)` with equal indices per entry.
pub(crate) fn history_items<S: AsRef<str>>(
    history: &[S],
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut previews = Vec::new();
    let mut fulls = Vec::new();
    let mut haystacks = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in history.iter().rev() {
        let text = entry.as_ref().trim();
        if text.is_empty() || !seen.insert(text) {
            continue;
        }
        previews.push(preview(text));
        haystacks.push(haystack(text));
        fulls.push(text.to_owned());
        if previews.len() >= MAX_HISTORY_SEARCH_ENTRIES {
            break;
        }
    }
    (previews, fulls, haystacks)
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
                let entry = picker
                    .selected_item_index()
                    .and_then(|index| search.full.get(index))
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
        let (items, full, _) = history_items(&history);
        assert_eq!(items, vec!["fix the build", "multi ⏎ line prompt"]);
        assert_eq!(full[1], "multi\nline prompt");
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
        let (items, full, haystacks) = history_items(&history);
        let mut picker = ListPicker::new();
        picker.set_items_with_haystacks(items, haystacks);
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

    #[test]
    fn search_covers_the_whole_entry_and_equal_previews_stay_distinct() {
        let shared = "p".repeat(300);
        let long_a = format!("{shared} {} needle tail", "a".repeat(300));
        let long_b = format!("{shared} {} other tail", "b".repeat(300));
        let history = [long_b.as_str(), long_a.as_str(), "short entry"];
        let (items, full, haystacks) = history_items(&history);
        assert_eq!(items.len(), 3);
        assert_eq!(items[1], items[2], "previews of the long entries collide");
        let mut picker = ListPicker::new();
        picker.set_items_with_haystacks(items, haystacks);
        picker.activate();
        let mut search = HistorySearch {
            draft: CompactString::default(),
            cursor: 0,
            full,
        };
        let mut buffer = CompactString::default();
        let mut cursor = 0;
        for c in "needle".chars() {
            handle_history_key(
                &mut buffer,
                &mut cursor,
                &mut picker,
                &mut search,
                key(KeyCode::Char(c)),
            );
        }
        assert_eq!(picker.len(), 1);
        // The row shows a window around the match, not the preview.
        assert!(picker.display_rows()[0].contains("needle"));
        handle_history_key(
            &mut buffer,
            &mut cursor,
            &mut picker,
            &mut search,
            key(KeyCode::Enter),
        );
        assert_eq!(buffer.as_str(), long_a);

        // Both long entries remain selectable when the query is empty.
        picker.activate();
        picker.select_next();
        picker.select_next();
        assert!(handle_history_key(
            &mut buffer,
            &mut cursor,
            &mut picker,
            &mut search,
            key(KeyCode::Enter)
        ));
        assert_eq!(buffer.as_str(), long_b);
    }
}
