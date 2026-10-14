//! Input-only modal state; no session, permission or transcript mutations.

use compact_str::CompactString;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum VimMode {
    #[default]
    Insert,
    Normal,
    Search,
}

#[derive(Clone, Debug, Default)]
pub(super) struct VimState {
    pub enabled: bool,
    pub mode: VimMode,
    pending_delete: bool,
    pub search: CompactString,
    pub last_search: Option<CompactString>,
    pub search_origin: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum VimAction {
    Pass,
    Consume,
    Left,
    Right,
    Up,
    Down,
    WordNext,
    WordPrevious,
    LineStart,
    LineEnd,
    Delete,
    DeleteLine,
    Undo,
    InsertAfter,
    InsertLineStart,
    InsertLineEnd,
    BeginSearch,
    Find,
    CancelSearch,
}

impl VimState {
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.mode = VimMode::Insert;
        self.pending_delete = false;
        self.search.clear();
    }

    pub fn label(&self) -> &'static str {
        if !self.enabled {
            return "";
        }
        match self.mode {
            VimMode::Insert => "vim:INSERT",
            VimMode::Normal => "vim:NORMAL",
            VimMode::Search => "vim:SEARCH",
        }
    }

    pub fn action(&mut self, key: KeyEvent) -> VimAction {
        if !self.enabled {
            return VimAction::Pass;
        }
        if self.mode == VimMode::Search {
            return match key.code {
                KeyCode::Esc => {
                    self.mode = VimMode::Normal;
                    VimAction::CancelSearch
                }
                KeyCode::Enter => {
                    self.mode = VimMode::Normal;
                    if !self.search.is_empty() {
                        self.last_search = Some(self.search.clone());
                    }
                    VimAction::Find
                }
                KeyCode::Backspace => {
                    self.search.pop();
                    VimAction::Consume
                }
                KeyCode::Char(c)
                    if !c.is_control()
                        && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
                        && self.search.len() + c.len_utf8() <= 256 =>
                {
                    self.search.push(c);
                    VimAction::Consume
                }
                _ => VimAction::Consume,
            };
        }
        if self.mode == VimMode::Insert {
            if key.code == KeyCode::Esc {
                self.mode = VimMode::Normal;
                self.pending_delete = false;
                return VimAction::Consume;
            }
            return VimAction::Pass;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            self.pending_delete = false;
            return VimAction::Pass;
        }
        let deleting = std::mem::take(&mut self.pending_delete);
        match key.code {
            KeyCode::Char('d') if deleting => VimAction::DeleteLine,
            KeyCode::Char('d') => {
                self.pending_delete = true;
                VimAction::Consume
            }
            KeyCode::Char('h') => VimAction::Left,
            KeyCode::Char('l') => VimAction::Right,
            KeyCode::Char('k') => VimAction::Up,
            KeyCode::Char('j') => VimAction::Down,
            KeyCode::Char('w') => VimAction::WordNext,
            KeyCode::Char('b') => VimAction::WordPrevious,
            KeyCode::Char('0') => VimAction::LineStart,
            KeyCode::Char('$') => VimAction::LineEnd,
            KeyCode::Char('x') => VimAction::Delete,
            KeyCode::Char('u') => VimAction::Undo,
            KeyCode::Char('i') => {
                self.mode = VimMode::Insert;
                VimAction::Consume
            }
            KeyCode::Char('a') => {
                self.mode = VimMode::Insert;
                VimAction::InsertAfter
            }
            KeyCode::Char('I') => {
                self.mode = VimMode::Insert;
                VimAction::InsertLineStart
            }
            KeyCode::Char('A') => {
                self.mode = VimMode::Insert;
                VimAction::InsertLineEnd
            }
            KeyCode::Char('/') => {
                self.search.clear();
                self.mode = VimMode::Search;
                VimAction::BeginSearch
            }
            KeyCode::Char('n') => VimAction::Find,
            KeyCode::Enter
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::Backspace
            | KeyCode::Delete => VimAction::Pass,
            _ => VimAction::Consume,
        }
    }
}

pub(super) fn next_word_start(text: &str, cursor: usize) -> usize {
    let mut iter = text[cursor..].char_indices().peekable();
    let Some((_, first)) = iter.peek().copied() else {
        return text.len();
    };
    let kind = char_kind(first);
    while iter.peek().is_some_and(|(_, c)| char_kind(*c) == kind) {
        iter.next();
    }
    while iter.peek().is_some_and(|(_, c)| c.is_whitespace()) {
        iter.next();
    }
    iter.peek()
        .map_or(text.len(), |(offset, _)| cursor + offset)
}

pub(super) fn previous_word_start(text: &str, cursor: usize) -> usize {
    let mut iter = text[..cursor].char_indices().rev().peekable();
    while iter.peek().is_some_and(|(_, c)| c.is_whitespace()) {
        iter.next();
    }
    let Some((mut start, first)) = iter.next() else {
        return 0;
    };
    let kind = char_kind(first);
    while let Some((offset, c)) = iter.peek().copied() {
        if char_kind(c) != kind {
            break;
        }
        start = offset;
        iter.next();
    }
    start
}

fn char_kind(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

pub(super) fn find_literal(text: &str, cursor: usize, query: &str) -> Option<usize> {
    if query.is_empty() {
        return None;
    }
    let after = text[cursor..]
        .chars()
        .next()
        .map_or(cursor, |c| cursor + c.len_utf8());
    text[after..]
        .find(query)
        .map(|offset| after + offset)
        .or_else(|| text.find(query))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn keys73_vim_modes_require_escape_and_complete_delete_operator() {
        let mut state = VimState::default();
        state.set_enabled(true);
        assert_eq!(state.action(key('h')), VimAction::Pass);
        assert_eq!(
            state.action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            VimAction::Consume
        );
        assert_eq!(state.label(), "vim:NORMAL");
        assert_eq!(state.action(key('d')), VimAction::Consume);
        assert_eq!(state.action(key('h')), VimAction::Left);
        assert_eq!(state.action(key('d')), VimAction::Consume);
        assert_eq!(state.action(key('d')), VimAction::DeleteLine);
        assert_eq!(state.action(key('i')), VimAction::Consume);
        assert_eq!(state.label(), "vim:INSERT");
    }

    #[test]
    fn keys73_vim_unicode_words_and_literal_search_keep_byte_boundaries() {
        let text = "雪猫 foo.bar 雪猫";
        let word = next_word_start(text, 0);
        assert_eq!(&text[word..], "foo.bar 雪猫");
        let punct = next_word_start(text, word);
        assert_eq!(&text[punct..], ".bar 雪猫");
        assert_eq!(previous_word_start(text, word), 0);
        assert_eq!(
            find_literal(text, 0, "雪猫"),
            Some(text.rfind("雪猫").unwrap())
        );
        assert_eq!(
            find_literal(text, text.rfind("雪猫").unwrap(), "雪猫"),
            Some(0)
        );
        assert_eq!(find_literal(text, 0, "missing"), None);
        // Wrapping also finds a match spanning the current caret.
        assert_eq!(find_literal("abc", 1, "abc"), Some(0));
        for cursor in [0, word, punct, text.len()] {
            assert!(text.is_char_boundary(next_word_start(text, cursor)));
        }
    }

    #[test]
    fn keys73_vim_search_is_bounded_and_escape_cancels() {
        let mut state = VimState::default();
        state.set_enabled(true);
        state.mode = VimMode::Normal;
        assert_eq!(state.action(key('/')), VimAction::BeginSearch);
        for _ in 0..200 {
            state.action(key('雪'));
        }
        assert!(state.search.len() <= 256);
        assert_eq!(
            state.action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            VimAction::CancelSearch
        );
        assert_eq!(state.last_search, None);
        state.set_enabled(false);
        assert_eq!(state.label(), "");
        assert_eq!(state.action(key('d')), VimAction::Pass);
    }
}
