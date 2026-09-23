//! `!<command>` completion: suggest shell commands the user ran before.
//!
//! The source is the input history (global chat history plus this session's
//! submissions), which records every `!command` line verbatim. Suggestions are
//! shown through a [`ListPicker`], whose ranking puts an exact match first,
//! then prefix matches, then word-boundary and other substring matches, with
//! ties kept in the order given here: most recent first.

use compact_str::CompactString;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::handlers::handle_prefixed_key;
use super::list::ListPicker;

/// The prefix that opens the picker at the start of the input.
pub(crate) const BANG_PREFIX: &str = "!";

/// Upper bound on distinct commands offered, newest first.
const MAX_BANG_COMMANDS: usize = 200;

/// Shell commands found in `history` (oldest entry first), without the
/// leading `!`, trimmed, newest first and deduplicated so each command appears
/// once at its most recent position. Empty and multi-line commands are
/// skipped because they cannot be shown or completed on one line.
pub(crate) fn bang_history<S: AsRef<str>>(history: &[S]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut commands = Vec::new();
    for entry in history.iter().rev() {
        let Some(command) = entry.as_ref().strip_prefix(BANG_PREFIX) else {
            continue;
        };
        let command = command.trim();
        if command.is_empty() || command.contains(['\n', '\r']) {
            continue;
        }
        if seen.insert(command.to_string()) {
            commands.push(command.to_string());
            if commands.len() >= MAX_BANG_COMMANDS {
                break;
            }
        }
    }
    commands
}

/// Keys while the `!` history picker is open. Unlike the other prefixed
/// pickers the typed text is the command itself, so nothing typed is ever
/// discarded: Esc only closes the picker, and Enter submits the input as typed
/// (returns `false`) when there is no suggestion or the highlight is exactly
/// what was typed. Backspace on the bare `!` deletes it and closes the picker.
pub fn handle_bang_key(
    buffer: &mut CompactString,
    cursor: &mut usize,
    picker: &mut ListPicker,
    key: KeyEvent,
) -> bool {
    let is_backspace = match key.code {
        KeyCode::Backspace => true,
        KeyCode::Char(c) => {
            c == '\x08' || (c == 'h' && key.modifiers.contains(KeyModifiers::CONTROL))
        }
        _ => false,
    };
    if is_backspace && picker.cursor == 0 {
        if buffer.starts_with(BANG_PREFIX) {
            *buffer = buffer[BANG_PREFIX.len()..].into();
            *cursor = 0;
        }
        picker.deactivate();
        return true;
    }
    match key.code {
        KeyCode::Esc => {
            picker.deactivate();
            true
        }
        KeyCode::Enter => {
            let takes_highlight = picker
                .selected_name()
                .is_some_and(|command| command != picker.query);
            if takes_highlight {
                handle_prefixed_key(buffer, cursor, picker, BANG_PREFIX, key)
            } else {
                picker.deactivate();
                false
            }
        }
        _ => handle_prefixed_key(buffer, cursor, picker, BANG_PREFIX, key),
    }
}
