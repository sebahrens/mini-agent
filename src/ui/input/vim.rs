//! Input-only modal state; no session, permission or transcript mutations.
//!
//! Normal mode is a small command parser: `[count] [operator] [count]
//! motion`, where the operators are `d`, `c` and `y` and the motions are
//! `h j k l w e b 0 ^ $ gg G f F t T` plus the text objects `iw` and `aw`.
//! `x X D C p P o O r ~ u .` are single commands. The parser only yields
//! typed [`VimAction`]s; [`InputEditor`] applies them to the buffer.

use super::{
    InputEditor, count_lines, cursor_to_line_col, line_col_to_cursor, line_end, line_start,
};
use compact_str::CompactString;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Largest repeat count accepted; bounds the work of `99999999w`.
const MAX_COUNT: usize = 10_000;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum VimMode {
    #[default]
    Insert,
    Normal,
    Search,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Operator {
    Delete,
    Change,
    Yank,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Motion {
    Left,
    Right,
    Up,
    Down,
    WordNext,
    WordEnd,
    WordPrevious,
    LineStart,
    FirstNonBlank,
    LineEnd,
    /// Zero-based line index; `usize::MAX` is the last line.
    GotoLine(usize),
    Find {
        ch: char,
        forward: bool,
        till: bool,
    },
    /// The doubled operator (`dd`, `cc`, `yy`): `count` whole lines.
    Line,
    InnerWord,
    AroundWord,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Stage {
    #[default]
    None,
    G,
    Find {
        forward: bool,
        till: bool,
    },
    Replace,
    Object {
        around: bool,
    },
}

#[derive(Clone, Copy, Debug, Default)]
struct Pending {
    count: Option<usize>,
    operator: Option<Operator>,
    operator_count: Option<usize>,
    stage: Stage,
}

impl Pending {
    fn is_idle(&self) -> bool {
        self.count.is_none()
            && self.operator.is_none()
            && self.operator_count.is_none()
            && self.stage == Stage::None
    }

    fn total_count(&self) -> usize {
        self.count
            .unwrap_or(1)
            .saturating_mul(self.operator_count.unwrap_or(1))
            .clamp(1, MAX_COUNT)
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct VimState {
    pub enabled: bool,
    pub mode: VimMode,
    pending: Pending,
    pub search: CompactString,
    pub last_search: Option<CompactString>,
    pub search_origin: usize,
    /// Keys of the change being typed (`cw` + text + Esc), and the last
    /// complete one that `.` replays.
    recording: Vec<KeyEvent>,
    edited: bool,
    last_change: Vec<KeyEvent>,
    replaying: bool,
    /// Text of the last delete or yank and whether it was whole lines.
    register: Option<(CompactString, bool)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum VimAction {
    Pass,
    Consume,
    Move {
        motion: Motion,
        count: usize,
    },
    Operate {
        operator: Operator,
        motion: Motion,
        count: usize,
    },
    Put {
        after: bool,
        count: usize,
    },
    Open {
        above: bool,
    },
    Replace {
        ch: char,
        count: usize,
    },
    ToggleCase {
        count: usize,
    },
    Repeat,
    Undo,
    InsertAfter,
    InsertLineStart,
    InsertLineEnd,
    BeginSearch,
    Find,
    CancelSearch,
}

impl VimAction {
    fn edits(self) -> bool {
        match self {
            VimAction::Operate { operator, .. } => operator != Operator::Yank,
            VimAction::Put { .. }
            | VimAction::Open { .. }
            | VimAction::Replace { .. }
            | VimAction::ToggleCase { .. }
            | VimAction::InsertAfter
            | VimAction::InsertLineStart
            | VimAction::InsertLineEnd => true,
            _ => false,
        }
    }
}

impl VimState {
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.mode = VimMode::Insert;
        self.pending = Pending::default();
        self.recording.clear();
        self.last_change.clear();
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
                self.record(key);
                self.mode = VimMode::Normal;
                self.pending = Pending::default();
                self.finish_change();
                return VimAction::Consume;
            }
            self.record(key);
            return VimAction::Pass;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            self.pending = Pending::default();
            return VimAction::Pass;
        }
        if self.pending.is_idle() && !self.replaying {
            self.recording.clear();
            self.edited = false;
        }
        self.record(key);
        let action = self.normal(key);
        if action.edits() || self.mode == VimMode::Insert {
            self.edited = true;
        }
        if self.pending.is_idle() && self.mode == VimMode::Normal {
            self.finish_change();
        }
        action
    }

    fn record(&mut self, key: KeyEvent) {
        if !self.replaying && self.recording.len() < 4096 {
            self.recording.push(key);
        }
    }

    /// A command finished in Normal mode: keep it for `.` if it edited.
    fn finish_change(&mut self) {
        if !self.replaying && self.edited && !self.recording.is_empty() {
            self.last_change = std::mem::take(&mut self.recording);
        }
        self.recording.clear();
        self.edited = false;
    }

    fn cancel(&mut self) -> VimAction {
        self.pending = Pending::default();
        VimAction::Consume
    }

    /// Complete a motion: a plain move, or the operand of a pending operator.
    fn motion(&mut self, motion: Motion) -> VimAction {
        let count = self.pending.total_count();
        self.motion_with(motion, count)
    }

    fn motion_with(&mut self, motion: Motion, count: usize) -> VimAction {
        let operator = self.pending.operator;
        self.pending = Pending::default();
        match operator {
            Some(operator) => {
                if operator == Operator::Change {
                    self.mode = VimMode::Insert;
                }
                VimAction::Operate {
                    operator,
                    motion,
                    count,
                }
            }
            None => VimAction::Move { motion, count },
        }
    }

    fn operate(&mut self, operator: Operator, motion: Motion) -> VimAction {
        let count = self.pending.total_count();
        self.pending = Pending::default();
        if operator == Operator::Change {
            self.mode = VimMode::Insert;
        }
        VimAction::Operate {
            operator,
            motion,
            count,
        }
    }

    fn normal(&mut self, key: KeyEvent) -> VimAction {
        match self.pending.stage {
            Stage::None => {}
            Stage::G => {
                return if key.code == KeyCode::Char('g') {
                    let line = self
                        .pending
                        .count
                        .or(self.pending.operator_count)
                        .map_or(0, |n| n.saturating_sub(1));
                    self.motion_with(Motion::GotoLine(line), 1)
                } else {
                    self.cancel()
                };
            }
            Stage::Find { forward, till } => {
                return match key.code {
                    KeyCode::Char(ch) => self.motion(Motion::Find { ch, forward, till }),
                    _ => self.cancel(),
                };
            }
            Stage::Replace => {
                return match key.code {
                    KeyCode::Char(ch) if !ch.is_control() => {
                        let count = self.pending.total_count();
                        self.pending = Pending::default();
                        VimAction::Replace { ch, count }
                    }
                    _ => self.cancel(),
                };
            }
            Stage::Object { around } => {
                return match key.code {
                    KeyCode::Char('w') => self.motion(if around {
                        Motion::AroundWord
                    } else {
                        Motion::InnerWord
                    }),
                    _ => self.cancel(),
                };
            }
        }
        let has_operator = self.pending.operator.is_some();
        match key.code {
            KeyCode::Esc => self.cancel(),
            KeyCode::Char(digit @ '0'..='9')
                if digit != '0'
                    || (if has_operator {
                        self.pending.operator_count.is_some()
                    } else {
                        self.pending.count.is_some()
                    }) =>
            {
                let digit = digit as usize - '0' as usize;
                let slot = if has_operator {
                    &mut self.pending.operator_count
                } else {
                    &mut self.pending.count
                };
                *slot = Some(
                    slot.unwrap_or(0)
                        .saturating_mul(10)
                        .saturating_add(digit)
                        .min(MAX_COUNT),
                );
                VimAction::Consume
            }
            KeyCode::Char(c @ ('d' | 'c' | 'y')) => {
                let operator = match c {
                    'd' => Operator::Delete,
                    'c' => Operator::Change,
                    _ => Operator::Yank,
                };
                match self.pending.operator {
                    None => {
                        self.pending.operator = Some(operator);
                        VimAction::Consume
                    }
                    Some(pending) if pending == operator => self.motion(Motion::Line),
                    Some(_) => self.cancel(),
                }
            }
            KeyCode::Char('h') => self.motion(Motion::Left),
            KeyCode::Char('l') => self.motion(Motion::Right),
            KeyCode::Char('k') => self.motion(Motion::Up),
            KeyCode::Char('j') => self.motion(Motion::Down),
            KeyCode::Char('w') => self.motion(Motion::WordNext),
            KeyCode::Char('e') => self.motion(Motion::WordEnd),
            KeyCode::Char('b') => self.motion(Motion::WordPrevious),
            KeyCode::Char('0') => self.motion(Motion::LineStart),
            KeyCode::Char('^') => self.motion(Motion::FirstNonBlank),
            KeyCode::Char('$') => self.motion(Motion::LineEnd),
            KeyCode::Char('G') => {
                let line = self
                    .pending
                    .count
                    .or(self.pending.operator_count)
                    .map_or(usize::MAX, |n| n.saturating_sub(1));
                self.motion_with(Motion::GotoLine(line), 1)
            }
            KeyCode::Char('g') => {
                self.pending.stage = Stage::G;
                VimAction::Consume
            }
            KeyCode::Char(c @ ('f' | 'F' | 't' | 'T')) => {
                self.pending.stage = Stage::Find {
                    forward: c.is_ascii_lowercase(),
                    till: matches!(c, 't' | 'T'),
                };
                VimAction::Consume
            }
            KeyCode::Char(c @ ('i' | 'a')) if has_operator => {
                self.pending.stage = Stage::Object { around: c == 'a' };
                VimAction::Consume
            }
            _ if has_operator => self.cancel(),
            KeyCode::Char('x') => self.operate(Operator::Delete, Motion::Right),
            KeyCode::Char('X') => self.operate(Operator::Delete, Motion::Left),
            KeyCode::Char('D') => self.operate(Operator::Delete, Motion::LineEnd),
            KeyCode::Char('C') => self.operate(Operator::Change, Motion::LineEnd),
            KeyCode::Char('p') | KeyCode::Char('P') => {
                let after = key.code == KeyCode::Char('p');
                let count = self.pending.total_count();
                self.pending = Pending::default();
                VimAction::Put { after, count }
            }
            KeyCode::Char('o') | KeyCode::Char('O') => {
                self.pending = Pending::default();
                self.mode = VimMode::Insert;
                VimAction::Open {
                    above: key.code == KeyCode::Char('O'),
                }
            }
            KeyCode::Char('r') => {
                self.pending.stage = Stage::Replace;
                VimAction::Consume
            }
            KeyCode::Char('~') => {
                let count = self.pending.total_count();
                self.pending = Pending::default();
                VimAction::ToggleCase { count }
            }
            KeyCode::Char('.') => {
                self.pending = Pending::default();
                if self.replaying || self.last_change.is_empty() {
                    VimAction::Consume
                } else {
                    VimAction::Repeat
                }
            }
            KeyCode::Char('u') => {
                self.pending = Pending::default();
                VimAction::Undo
            }
            KeyCode::Char('i') => {
                self.pending = Pending::default();
                self.mode = VimMode::Insert;
                VimAction::Consume
            }
            KeyCode::Char('a') => {
                self.pending = Pending::default();
                self.mode = VimMode::Insert;
                VimAction::InsertAfter
            }
            KeyCode::Char('I') => {
                self.pending = Pending::default();
                self.mode = VimMode::Insert;
                VimAction::InsertLineStart
            }
            KeyCode::Char('A') => {
                self.pending = Pending::default();
                self.mode = VimMode::Insert;
                VimAction::InsertLineEnd
            }
            KeyCode::Char('/') => {
                self.pending = Pending::default();
                self.search.clear();
                self.mode = VimMode::Search;
                VimAction::BeginSearch
            }
            KeyCode::Char('n') => {
                self.pending = Pending::default();
                VimAction::Find
            }
            KeyCode::Enter
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::Backspace
            | KeyCode::Delete => {
                self.pending = Pending::default();
                VimAction::Pass
            }
            _ => self.cancel(),
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

/// Start of the last character of the word at or after the one following
/// `cursor` (Vim `e`); `cursor` when there is none.
pub(super) fn word_end(text: &str, cursor: usize) -> usize {
    let Some(current) = text[cursor..].chars().next() else {
        return text.len();
    };
    let after = cursor + current.len_utf8();
    let mut iter = text[after..].char_indices().peekable();
    while iter.peek().is_some_and(|(_, c)| c.is_whitespace()) {
        iter.next();
    }
    let Some((first, c)) = iter.next() else {
        return cursor;
    };
    let kind = char_kind(c);
    let mut last = first;
    while let Some((offset, c)) = iter.peek().copied() {
        if char_kind(c) != kind {
            break;
        }
        last = offset;
        iter.next();
    }
    after + last
}

/// First non-blank position of the line holding `cursor` (its end when the
/// line is blank).
pub(super) fn first_non_blank(text: &str, cursor: usize) -> usize {
    let start = line_start(text, cursor);
    let end = line_end(text, cursor);
    text[start..end]
        .char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map_or(end, |(offset, _)| start + offset)
}

/// The `count`-th `ch` after (or before) `cursor` on its line, or the
/// character next to it for `till`.
pub(super) fn find_char(
    text: &str,
    cursor: usize,
    ch: char,
    forward: bool,
    till: bool,
    count: usize,
) -> Option<usize> {
    let start = line_start(text, cursor);
    let end = line_end(text, cursor);
    let mut remaining = count.max(1);
    if forward {
        let from = cursor + text[cursor..end].chars().next().map_or(0, char::len_utf8);
        for (offset, c) in text[from..end].char_indices() {
            if c == ch {
                remaining -= 1;
                if remaining == 0 {
                    let at = from + offset;
                    return Some(if till {
                        text[..at].char_indices().next_back().map_or(at, |(i, _)| i)
                    } else {
                        at
                    });
                }
            }
        }
    } else {
        for (offset, c) in text[start..cursor].char_indices().rev() {
            if c == ch {
                remaining -= 1;
                if remaining == 0 {
                    let at = start + offset;
                    return Some(if till { at + c.len_utf8() } else { at });
                }
            }
        }
    }
    None
}

/// Range of the word under `cursor` (`iw`), plus surrounding blanks
/// (`aw`). Runs never cross a line break.
pub(super) fn word_object(text: &str, cursor: usize, around: bool) -> (usize, usize) {
    let Some(current) = text[cursor..].chars().next().filter(|c| *c != '\n') else {
        return (cursor, cursor);
    };
    let kind = char_kind(current);
    let same = |c: char| c != '\n' && char_kind(c) == kind;
    let mut start = cursor;
    while let Some(c) = text[..start].chars().next_back().filter(|c| same(*c)) {
        start -= c.len_utf8();
    }
    let mut end = cursor;
    while let Some(c) = text[end..].chars().next().filter(|c| same(*c)) {
        end += c.len_utf8();
    }
    if around {
        let blank = |c: char| c != '\n' && c.is_whitespace();
        if kind == 0 {
            // Blanks plus the word after them.
            let word_kind = text[end..]
                .chars()
                .next()
                .filter(|c| *c != '\n')
                .map(char_kind);
            if let Some(word_kind) = word_kind {
                while let Some(c) = text[end..]
                    .chars()
                    .next()
                    .filter(|c| *c != '\n' && char_kind(*c) == word_kind)
                {
                    end += c.len_utf8();
                }
            }
        } else {
            let before = end;
            while let Some(c) = text[end..].chars().next().filter(|c| blank(*c)) {
                end += c.len_utf8();
            }
            if end == before {
                while let Some(c) = text[..start].chars().next_back().filter(|c| blank(*c)) {
                    start -= c.len_utf8();
                }
            }
        }
    }
    (start, end)
}

/// End of the word run starting at `cursor` (exclusive), for `cw`.
fn run_end(text: &str, cursor: usize) -> usize {
    let Some(first) = text[cursor..].chars().next() else {
        return cursor;
    };
    let kind = char_kind(first);
    let mut end = cursor;
    while let Some(c) = text[end..].chars().next().filter(|c| char_kind(*c) == kind) {
        end += c.len_utf8();
    }
    end
}

impl InputEditor {
    fn line_index(&self, pos: usize) -> usize {
        cursor_to_line_col(&self.buffer, pos).0
    }

    fn line_start_of(&self, line: usize) -> usize {
        line_col_to_cursor(&self.buffer, line, 0)
    }

    /// Where `motion` repeated `count` times lands from the cursor; `None`
    /// when it cannot move (for example `f` without a match). With
    /// `operand`, motions do not cross line breaks.
    fn vim_target(&self, motion: Motion, count: usize, operand: bool) -> Option<usize> {
        let buffer = self.buffer.as_str();
        let mut pos = self.cursor;
        match motion {
            Motion::Left => {
                for _ in 0..count {
                    if pos == 0 || (operand && pos == line_start(buffer, pos)) {
                        break;
                    }
                    pos = self.prev_boundary(pos);
                }
            }
            Motion::Right => {
                for _ in 0..count {
                    if pos >= buffer.len() || (operand && buffer.as_bytes()[pos] == b'\n') {
                        break;
                    }
                    pos = self.next_boundary(pos);
                }
            }
            Motion::Up | Motion::Down => {
                let (line, col) = cursor_to_line_col(buffer, pos);
                let last = count_lines(buffer) - 1;
                let line = if motion == Motion::Up {
                    line.saturating_sub(count)
                } else {
                    line.saturating_add(count).min(last)
                };
                pos = line_col_to_cursor(buffer, line, col);
            }
            Motion::WordNext => {
                for _ in 0..count {
                    if pos >= buffer.len() {
                        break;
                    }
                    pos = next_word_start(buffer, pos);
                }
                if operand {
                    pos = pos.min(line_end(buffer, self.cursor));
                }
            }
            Motion::WordEnd => {
                for _ in 0..count {
                    pos = word_end(buffer, pos);
                }
            }
            Motion::WordPrevious => {
                for _ in 0..count {
                    pos = previous_word_start(buffer, pos);
                }
            }
            Motion::LineStart => pos = line_start(buffer, pos),
            Motion::FirstNonBlank => pos = first_non_blank(buffer, pos),
            Motion::LineEnd => {
                let (line, _) = cursor_to_line_col(buffer, pos);
                let last = count_lines(buffer) - 1;
                let target = (line + count - 1).min(last);
                pos = line_end(buffer, line_col_to_cursor(buffer, target, 0));
            }
            Motion::GotoLine(line) => {
                let line = line.min(count_lines(buffer) - 1);
                pos = line_col_to_cursor(buffer, line, 0);
            }
            Motion::Find { ch, forward, till } => {
                pos = find_char(buffer, pos, ch, forward, till, count)?;
            }
            Motion::Line | Motion::InnerWord | Motion::AroundWord => return None,
        }
        Some(pos)
    }

    /// `(start, end, whole lines)` an operator acts on. Whole-line spans run
    /// from the first line's start to the last line's end, newline excluded.
    fn vim_span(
        &self,
        operator: Operator,
        motion: Motion,
        count: usize,
    ) -> Option<(usize, usize, bool)> {
        let buffer = self.buffer.as_str();
        let lines = |a: usize, b: usize| {
            let (first, last) = (a.min(b), a.max(b));
            let start = self.line_start_of(first);
            let end = line_end(buffer, self.line_start_of(last));
            Some((start, end, true))
        };
        let here = self.line_index(self.cursor);
        match motion {
            Motion::Line => lines(here, (here + count - 1).min(count_lines(buffer) - 1)),
            Motion::Up | Motion::Down | Motion::GotoLine(_) => {
                let target = self.vim_target(motion, count, true)?;
                let there = self.line_index(target);
                (there != here || matches!(motion, Motion::GotoLine(_))).then_some(())?;
                lines(here, there)
            }
            Motion::InnerWord | Motion::AroundWord => {
                let (start, end) = word_object(buffer, self.cursor, motion == Motion::AroundWord);
                Some((start, end, false))
            }
            Motion::WordNext
                if operator == Operator::Change
                    && buffer[self.cursor..]
                        .chars()
                        .next()
                        .is_some_and(|c| !c.is_whitespace()) =>
            {
                // `cw` changes to the end of the word, like `ce`.
                let mut end = run_end(buffer, self.cursor);
                for _ in 1..count {
                    let mut at = end;
                    while let Some(c) = buffer[at..].chars().next().filter(|c| c.is_whitespace()) {
                        at += c.len_utf8();
                    }
                    end = run_end(buffer, at);
                }
                Some((self.cursor, end, false))
            }
            _ => {
                let target = self.vim_target(motion, count, true)?;
                let (mut start, mut end) = (self.cursor.min(target), self.cursor.max(target));
                let inclusive = matches!(
                    motion,
                    Motion::WordEnd | Motion::Find { forward: true, .. }
                );
                if inclusive && end < buffer.len() {
                    end = self.next_boundary(end);
                }
                if start > end {
                    start = end;
                }
                Some((start, end, false))
            }
        }
    }

    pub(super) fn vim_move(&mut self, motion: Motion, count: usize) {
        if let Some(target) = self.vim_target(motion, count, false) {
            self.cursor = target;
        }
    }

    pub(super) fn vim_operate(&mut self, operator: Operator, motion: Motion, count: usize) {
        let Some((start, end, linewise)) = self.vim_span(operator, motion, count) else {
            if operator == Operator::Change {
                self.vim.mode = VimMode::Normal;
            }
            return;
        };
        let text: CompactString = self.buffer[start..end].into();
        if operator == Operator::Yank {
            if !text.is_empty() || linewise {
                self.set_register(text, linewise);
            }
            return;
        }
        if linewise && operator == Operator::Delete {
            // Take one line break with the lines so none is left behind.
            if end < self.buffer.len() {
                self.buffer.replace_range(start..end + 1, "");
                self.cursor = start.min(self.buffer.len());
            } else if start > 0 {
                self.buffer.replace_range(start - 1..end, "");
                self.cursor = line_start(&self.buffer, start - 1);
            } else {
                self.buffer.replace_range(start..end, "");
                self.cursor = 0;
            }
        } else {
            self.buffer.replace_range(start..end, "");
            self.cursor = start;
        }
        if !text.is_empty() || linewise {
            self.set_register(text, linewise);
        }
    }

    fn set_register(&mut self, text: CompactString, linewise: bool) {
        self.vim.register = Some((text.clone(), linewise));
        self.push_kill(text);
    }

    pub(super) fn vim_put(&mut self, after: bool, count: usize) {
        let (text, linewise) = match (&self.vim.register, self.kill_ring.first()) {
            (Some((text, linewise)), Some(top)) if text == top => (text.clone(), *linewise),
            (_, Some(top)) => (top.clone(), false),
            _ => return,
        };
        let count = count.clamp(1, 100);
        if linewise {
            let block = vec![text.as_str(); count].join("\n");
            if after {
                let at = line_end(&self.buffer, self.cursor);
                self.buffer.insert_str(at, &format!("\n{block}"));
                self.cursor = at + 1;
            } else {
                let at = line_start(&self.buffer, self.cursor);
                self.buffer.insert_str(at, &format!("{block}\n"));
                self.cursor = at;
            }
        } else {
            let at = if after && self.cursor < line_end(&self.buffer, self.cursor) {
                self.next_boundary(self.cursor)
            } else {
                self.cursor
            };
            let block = text.repeat(count);
            self.buffer.insert_str(at, &block);
            self.cursor = self.prev_boundary(at + block.len());
        }
    }

    pub(super) fn vim_open(&mut self, above: bool) {
        if above {
            let at = line_start(&self.buffer, self.cursor);
            self.buffer.insert(at, '\n');
            self.cursor = at;
        } else {
            let at = line_end(&self.buffer, self.cursor);
            self.buffer.insert(at, '\n');
            self.cursor = at + 1;
        }
    }

    pub(super) fn vim_replace(&mut self, ch: char, count: usize) {
        let end = line_end(&self.buffer, self.cursor);
        let mut at = self.cursor;
        for _ in 0..count {
            if at >= end {
                return;
            }
            at = self.next_boundary(at);
        }
        if at > end {
            return;
        }
        let replacement: String = std::iter::repeat_n(ch, count).collect();
        let first = self.cursor;
        self.buffer.replace_range(first..at, &replacement);
        self.cursor = first + replacement.len() - ch.len_utf8();
    }

    pub(super) fn vim_toggle_case(&mut self, count: usize) {
        let end = line_end(&self.buffer, self.cursor);
        let mut at = self.cursor;
        for _ in 0..count {
            if at >= end {
                break;
            }
            let Some(c) = self.buffer[at..].chars().next() else {
                break;
            };
            let toggled: String = if c.is_lowercase() {
                c.to_uppercase().collect()
            } else if c.is_uppercase() {
                c.to_lowercase().collect()
            } else {
                c.to_string()
            };
            let len = c.len_utf8();
            self.buffer.replace_range(at..at + len, &toggled);
            at += toggled.len();
        }
        self.cursor = at.min(self.buffer.len());
    }

    /// Replay the last change (`.`). Submitting keys are skipped so a
    /// replayed insert can never send the draft.
    pub(super) fn vim_repeat(&mut self) {
        let keys = self.vim.last_change.clone();
        self.vim.replaying = true;
        for key in keys {
            self.clamp_cursor();
            let action = self.vim.action(key);
            if action == VimAction::Pass {
                let submits = key.code == KeyCode::Enter
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT);
                if !submits && let Some(event) = self.keybindings.effective_editor_event(key) {
                    let _ = self.handle_editor_key(event);
                }
            } else {
                self.apply_vim_action(action);
            }
        }
        self.vim.replaying = false;
    }
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
        assert_eq!(state.action(key('h')), VimAction::Move { motion: Motion::Left, count: 1 });
        assert_eq!(state.action(key('d')), VimAction::Consume);
        assert_eq!(state.action(key('h')), VimAction::Operate {
            operator: Operator::Delete,
            motion: Motion::Left,
            count: 1,
        });
        assert_eq!(state.action(key('d')), VimAction::Consume);
        assert_eq!(state.action(key('d')), VimAction::Operate {
            operator: Operator::Delete,
            motion: Motion::Line,
            count: 1,
        });
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
