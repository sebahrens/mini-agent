use compact_str::CompactString;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::file::FilePicker;
use super::list::ListPicker;
use super::models::ModelsPicker;

use crate::ui::input::cursor::prev_char_boundary;
use crate::ui::input::{Picker, is_modifier_chord};

/// Ctrl+W in a picker: delete the typed query (it is one word), or, when the
/// query is already empty, act like Backspace and drop the trigger.
fn is_ctrl_w(key: KeyEvent) -> bool {
    is_modifier_chord(key)
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('w' | 'W'))
}

const BACKSPACE: KeyEvent = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);

/// Replace the `@<query>` span that starts at byte offset `at` with
/// `replacement` and return the byte cursor just after the inserted text.
///
/// Every offset here is a byte offset: `at` comes from `str::rfind`, and
/// `query_len` is `picker.query.len()` (bytes). Slicing by chars with these
/// values corrupted any buffer containing multi-byte text before the `@`.
fn replace_at_span(
    buffer: &mut CompactString,
    at: usize,
    query_len: usize,
    replacement: &str,
) -> usize {
    let before = &buffer[..at];
    let after = buffer.get(at + 1 + query_len..).unwrap_or("");
    let mut next = String::with_capacity(before.len() + replacement.len() + after.len());
    next.push_str(before);
    next.push_str(replacement);
    next.push_str(after);
    *buffer = next.into();
    at + replacement.len()
}

pub fn handle_file_key(
    buffer: &mut CompactString,
    cursor: &mut usize,
    picker: &mut FilePicker,
    key: KeyEvent,
) -> bool {
    match key.code {
        KeyCode::Char(c)
            if c == '\x08' || (c == 'h' && key.modifiers.contains(KeyModifiers::CONTROL)) =>
        {
            if picker.cursor > 0 {
                picker.backspace();
                *cursor = prev_char_boundary(buffer, *cursor);
                buffer.remove(*cursor);
            } else {
                if let Some(at) = buffer.rfind('@') {
                    *cursor = replace_at_span(buffer, at, 0, "");
                }
                picker.deactivate();
            }
            true
        }
        _ if is_ctrl_w(key) => {
            if picker.cursor == 0 {
                return handle_file_key(buffer, cursor, picker, BACKSPACE);
            }
            while picker.cursor > 0 && !picker.query.is_empty() {
                handle_file_key(buffer, cursor, picker, BACKSPACE);
            }
            true
        }
        // Other Ctrl/Alt chords must not type their letter into the query.
        KeyCode::Char(_) if is_modifier_chord(key) => true,
        // A space ends the mention: keep what was typed, close the picker.
        KeyCode::Char(' ') => {
            picker.deactivate();
            buffer.insert(*cursor, ' ');
            *cursor += 1;
            true
        }
        KeyCode::Char(c) => {
            picker.char_input(c);
            buffer.insert(*cursor, c);
            *cursor += c.len_utf8();
            true
        }
        KeyCode::Backspace => {
            if picker.cursor > 0 {
                picker.backspace();
                *cursor = prev_char_boundary(buffer, *cursor);
                buffer.remove(*cursor);
                true
            } else {
                if let Some(at) = buffer.rfind('@') {
                    *cursor = replace_at_span(buffer, at, 0, "");
                }
                picker.deactivate();
                true
            }
        }
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
            picker.select_prev();
            true
        }
        KeyCode::BackTab | KeyCode::Up => {
            picker.select_prev();
            true
        }
        KeyCode::Down => {
            picker.select_next();
            true
        }
        KeyCode::Tab if picker.matches.is_empty() => true,
        KeyCode::Enter | KeyCode::Tab => {
            if let Some(path) = picker.selected_path() {
                let path_str = path.to_string_lossy().to_string();
                if let Some(at) = buffer.rfind('@') {
                    *cursor = replace_at_span(buffer, at, picker.query.len(), &path_str);
                }
            }
            picker.deactivate();
            true
        }
        KeyCode::Esc => {
            if let Some(at) = buffer.rfind('@') {
                *cursor = replace_at_span(buffer, at, picker.query.len(), "");
            }
            picker.deactivate();
            true
        }
        _ => false,
    }
}

pub struct CommandPickerCtx<'a> {
    pub prompt_names: &'a [String],
    pub agent_names: &'a [String],
    pub theme_names: &'a [String],
    pub quick_model_names: &'a [String],
    pub live_model_names: &'a [String],
    pub provider_names: &'a [String],
    /// The active security mode, or `None` when no permission system is
    /// running (then `/mode` has no picker and submits as text).
    pub security_mode: Option<crate::permission::SecurityMode>,
}

/// Backspace in the command picker. With a query it deletes one query
/// character; on the bare slash it deletes the slash itself and closes the
/// picker, so the input is plain again and the next `/` reopens completion.
fn command_backspace(buffer: &mut CompactString, cursor: &mut usize, picker: &mut ListPicker) {
    if picker.cursor > 0 {
        picker.backspace();
        let byte_in_query = picker
            .query
            .char_indices()
            .nth(picker.cursor)
            .map(|(i, _)| i)
            .unwrap_or(picker.query.len());
        let remove_pos = 1 + byte_in_query;
        if remove_pos < buffer.len() {
            buffer.remove(remove_pos);
        }
        *cursor = prev_char_boundary(buffer, *cursor);
    } else {
        if buffer.starts_with('/') {
            let after_offset = (1 + picker.query.len()).min(buffer.len());
            *buffer = buffer[after_offset..].into();
            *cursor = 0;
        }
        picker.deactivate();
    }
}

/// Whether accepting `command` hands over to a second picker for its argument.
fn opens_sub_picker(command: &str, ctx: &CommandPickerCtx) -> bool {
    match command {
        "/prompt" => !ctx.prompt_names.is_empty(),
        "/agent" => !ctx.agent_names.is_empty(),
        "/models" => !(ctx.quick_model_names.is_empty() && ctx.live_model_names.is_empty()),
        "/theme" => !ctx.theme_names.is_empty(),
        "/provider" => !ctx.provider_names.is_empty(),
        "/queue" => true,
        "/mode" => ctx.security_mode.is_some(),
        _ => false,
    }
}

/// Replace the typed `/query` with the highlighted command plus a space, and
/// open the argument picker when the command has one.
fn accept_command(
    buffer: &mut CompactString,
    cursor: &mut usize,
    ctx: &CommandPickerCtx,
    picker: &mut ListPicker,
) -> (bool, Option<Picker>) {
    if let Some(cmd) = picker.selected_name() {
        let selected = cmd.to_string();
        let slash_pos = buffer.find('/').unwrap_or(0);
        let before = &buffer[..slash_pos];
        let after_offset = slash_pos + 1 + picker.query.len();
        let after = &buffer[after_offset.min(buffer.len())..];
        let insertion = if after.is_empty() || after.starts_with(' ') {
            format!("{} ", selected)
        } else {
            format!("{}{}", selected, after)
        };
        let new_cursor = before.len() + selected.len() + 1;
        let replacement = format!("{}{}", before, insertion);
        *buffer = replacement.into();
        *cursor = new_cursor;

        if opens_sub_picker(&selected, ctx) {
            picker.deactivate();
            let sub = match selected.as_str() {
                "/models" => {
                    let mut mp = ModelsPicker::new();
                    mp.set_groups(
                        ctx.quick_model_names.to_vec(),
                        ctx.live_model_names.to_vec(),
                    );
                    mp.activate();
                    Picker::Models(mp)
                }
                "/mode" => Picker::Prefixed(mode_picker(ctx.security_mode), "/mode "),
                other => {
                    let (items, prefix): (Vec<String>, &'static str) = match other {
                        "/prompt" => (ctx.prompt_names.to_vec(), "/prompt "),
                        "/agent" => (ctx.agent_names.to_vec(), "/agent "),
                        "/theme" => (ctx.theme_names.to_vec(), "/theme "),
                        "/provider" => (ctx.provider_names.to_vec(), "/provider "),
                        _ => (
                            vec!["ls".to_string(), "clear".to_string(), "pop".to_string()],
                            "/queue ",
                        ),
                    };
                    let mut lp = ListPicker::new();
                    lp.set_items(items);
                    lp.activate();
                    Picker::Prefixed(lp, prefix)
                }
            };
            return (true, Some(sub));
        }
    }
    picker.deactivate();
    (true, None)
}

/// The `/mode` argument picker: every security mode with its one-line
/// description, the current one marked and highlighted.
pub(crate) fn mode_picker(current: Option<crate::permission::SecurityMode>) -> ListPicker {
    let mut picker = ListPicker::new();
    picker.set_described_items(
        crate::permission::SecurityMode::all()
            .map(|mode| (mode.to_string(), mode.description().to_string()))
            .collect(),
    );
    picker.set_current(current.map(|mode| mode.to_string()));
    picker.activate();
    picker
}

/// Keys while the slash-command picker is open. A `false` result means the
/// key was not consumed: Enter on a command typed in full closes the picker
/// and returns `false`, so the caller's normal Enter submits the input.
pub fn handle_command_key(
    buffer: &mut CompactString,
    cursor: &mut usize,
    ctx: &CommandPickerCtx,
    picker: &mut ListPicker,
    key: KeyEvent,
) -> (bool, Option<Picker>) {
    match key.code {
        KeyCode::Char(c)
            if c == '\x08' || (c == 'h' && key.modifiers.contains(KeyModifiers::CONTROL)) =>
        {
            command_backspace(buffer, cursor, picker);
            (true, None)
        }
        _ if is_ctrl_w(key) => {
            if picker.cursor == 0 {
                command_backspace(buffer, cursor, picker);
            }
            while picker.cursor > 0 && !picker.query.is_empty() {
                command_backspace(buffer, cursor, picker);
            }
            (true, None)
        }
        KeyCode::Char(_) if is_modifier_chord(key) => (true, None),
        KeyCode::Char(c) => {
            picker.char_input(c);
            let byte_in_query = picker
                .query
                .char_indices()
                .nth(picker.cursor.saturating_sub(1))
                .map(|(i, _)| i)
                .unwrap_or(picker.query.len());
            let pos = 1 + byte_in_query;
            buffer.insert(pos, c);
            *cursor += c.len_utf8();
            (true, None)
        }
        KeyCode::Backspace => {
            command_backspace(buffer, cursor, picker);
            (true, None)
        }
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
            picker.select_prev();
            (true, None)
        }
        KeyCode::BackTab | KeyCode::Up => {
            picker.select_prev();
            (true, None)
        }
        KeyCode::Down => {
            picker.select_next();
            (true, None)
        }
        KeyCode::Tab => {
            if picker.matches.is_empty() {
                return (true, None);
            }
            accept_command(buffer, cursor, ctx, picker)
        }
        KeyCode::Enter => {
            let typed_in_full = picker.selected_name().is_some_and(|cmd| {
                cmd.strip_prefix('/') == Some(picker.query.as_str())
                    && buffer.as_str() == cmd
                    && !opens_sub_picker(cmd, ctx)
            });
            if typed_in_full {
                picker.deactivate();
                return (false, None);
            }
            accept_command(buffer, cursor, ctx, picker)
        }
        KeyCode::Esc => {
            let slash_pos = buffer.find('/').unwrap_or(0);
            let before = &buffer[..slash_pos];
            let after_offset = slash_pos + 1 + picker.query.len();
            let after = &buffer[after_offset.min(buffer.len())..];
            let replacement = format!("{}/{}", before, after);
            *buffer = replacement.into();
            *cursor = slash_pos + 1;
            picker.deactivate();
            (true, None)
        }
        _ => (false, None),
    }
}

pub fn handle_prefixed_key(
    buffer: &mut CompactString,
    cursor: &mut usize,
    picker: &mut ListPicker,
    prefix: &str,
    key: KeyEvent,
) -> bool {
    let prefix_len = prefix.len();
    match key.code {
        KeyCode::Char(c)
            if c == '\x08' || (c == 'h' && key.modifiers.contains(KeyModifiers::CONTROL)) =>
        {
            if picker.cursor > 0 {
                picker.backspace();
                let byte_in_query = picker
                    .query
                    .char_indices()
                    .nth(picker.cursor)
                    .map(|(i, _)| i)
                    .unwrap_or(picker.query.len());
                let remove_pos = prefix_len + byte_in_query;
                if remove_pos < buffer.len() {
                    buffer.remove(remove_pos);
                }
                *cursor = prev_char_boundary(buffer, *cursor);
            } else {
                let after_offset = prefix_len + picker.query.chars().count();
                if buffer.len() >= after_offset {
                    let before: String = buffer.chars().take(prefix_len).collect();
                    let after: String = buffer.chars().skip(after_offset).collect();
                    *buffer = format!("{}{}", before, after).into();
                    *cursor = prefix_len;
                }
                picker.deactivate();
            }
            true
        }
        _ if is_ctrl_w(key) => {
            if picker.cursor == 0 {
                return handle_prefixed_key(buffer, cursor, picker, prefix, BACKSPACE);
            }
            while picker.cursor > 0 && !picker.query.is_empty() {
                handle_prefixed_key(buffer, cursor, picker, prefix, BACKSPACE);
            }
            true
        }
        KeyCode::Char(_) if is_modifier_chord(key) => true,
        KeyCode::Char(c) => {
            picker.char_input(c);
            let byte_in_query = picker
                .query
                .char_indices()
                .nth(picker.cursor.saturating_sub(1))
                .map(|(i, _)| i)
                .unwrap_or(picker.query.len());
            let insert_pos = prefix_len + byte_in_query;
            buffer.insert(insert_pos, c);
            *cursor += c.len_utf8();
            true
        }
        KeyCode::Backspace => {
            if picker.cursor > 0 {
                picker.backspace();
                let byte_in_query = picker
                    .query
                    .char_indices()
                    .nth(picker.cursor)
                    .map(|(i, _)| i)
                    .unwrap_or(picker.query.len());
                let remove_pos = prefix_len + byte_in_query;
                if remove_pos < buffer.len() {
                    buffer.remove(remove_pos);
                }
                *cursor = prev_char_boundary(buffer, *cursor);
                true
            } else {
                let after_offset = prefix_len + picker.query.chars().count();
                if buffer.len() >= after_offset {
                    let before: String = buffer.chars().take(prefix_len).collect();
                    let after: String = buffer.chars().skip(after_offset).collect();
                    *buffer = format!("{}{}", before, after).into();
                    *cursor = prefix_len;
                }
                picker.deactivate();
                true
            }
        }
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
            picker.select_prev();
            true
        }
        KeyCode::BackTab | KeyCode::Up => {
            picker.select_prev();
            true
        }
        KeyCode::Down => {
            picker.select_next();
            true
        }
        KeyCode::Tab if picker.matches.is_empty() => true,
        KeyCode::Enter | KeyCode::Tab => {
            if let Some(name) = picker.selected_name() {
                let after_offset = prefix_len + picker.query.chars().count();
                let before: String = buffer.chars().take(prefix_len).collect();
                let after: String = buffer.chars().skip(after_offset).collect();
                *buffer = format!("{}{}{}", before, name, after).into();
                *cursor = prefix_len + name.len();
            }
            picker.deactivate();
            true
        }
        KeyCode::Esc => {
            let after_offset = prefix_len + picker.query.chars().count();
            if buffer.len() >= after_offset {
                let before: String = buffer.chars().take(prefix_len).collect();
                let after: String = buffer.chars().skip(after_offset).collect();
                *buffer = format!("{}{}", before, after).into();
                *cursor = prefix_len;
            }
            picker.deactivate();
            true
        }
        _ => false,
    }
}

pub fn handle_models_key(
    buffer: &mut CompactString,
    cursor: &mut usize,
    picker: &mut ModelsPicker,
    key: KeyEvent,
) -> bool {
    let prefix = "/models ";
    let prefix_len = prefix.len();
    match key.code {
        KeyCode::Char(c)
            if c == '\x08' || (c == 'h' && key.modifiers.contains(KeyModifiers::CONTROL)) =>
        {
            if picker.cursor > 0 {
                picker.backspace();
                let byte_in_query = picker
                    .query
                    .char_indices()
                    .nth(picker.cursor)
                    .map(|(i, _)| i)
                    .unwrap_or(picker.query.len());
                let remove_pos = prefix_len + byte_in_query;
                if remove_pos < buffer.len() {
                    buffer.remove(remove_pos);
                }
                *cursor = prev_char_boundary(buffer, *cursor);
            } else {
                let after_offset = prefix_len + picker.query.chars().count();
                if buffer.len() >= after_offset {
                    let before: String = buffer.chars().take(prefix_len).collect();
                    let after: String = buffer.chars().skip(after_offset).collect();
                    *buffer = format!("{}{}", before, after).into();
                    *cursor = prefix_len;
                }
                picker.deactivate();
            }
            true
        }
        _ if is_ctrl_w(key) => {
            if picker.cursor == 0 {
                return handle_models_key(buffer, cursor, picker, BACKSPACE);
            }
            while picker.cursor > 0 && !picker.query.is_empty() {
                handle_models_key(buffer, cursor, picker, BACKSPACE);
            }
            true
        }
        KeyCode::Char(_) if is_modifier_chord(key) => true,
        KeyCode::Char(c) => {
            picker.char_input(c);
            let byte_in_query = picker
                .query
                .char_indices()
                .nth(picker.cursor.saturating_sub(1))
                .map(|(i, _)| i)
                .unwrap_or(picker.query.len());
            let insert_pos = prefix_len + byte_in_query;
            buffer.insert(insert_pos, c);
            *cursor += c.len_utf8();
            true
        }
        KeyCode::Backspace => {
            if picker.cursor > 0 {
                picker.backspace();
                let byte_in_query = picker
                    .query
                    .char_indices()
                    .nth(picker.cursor)
                    .map(|(i, _)| i)
                    .unwrap_or(picker.query.len());
                let remove_pos = prefix_len + byte_in_query;
                if remove_pos < buffer.len() {
                    buffer.remove(remove_pos);
                }
                *cursor = prev_char_boundary(buffer, *cursor);
                true
            } else {
                let after_offset = prefix_len + picker.query.chars().count();
                if buffer.len() >= after_offset {
                    let before: String = buffer.chars().take(prefix_len).collect();
                    let after: String = buffer.chars().skip(after_offset).collect();
                    *buffer = format!("{}{}", before, after).into();
                    *cursor = prefix_len;
                }
                picker.deactivate();
                true
            }
        }
        KeyCode::Tab | KeyCode::BackTab => {
            picker.toggle_group();
            true
        }
        KeyCode::Up => {
            picker.select_prev();
            true
        }
        KeyCode::Down => {
            picker.select_next();
            true
        }
        KeyCode::Enter => {
            if let Some(name) = picker.selected_name() {
                let after_offset = prefix_len + picker.query.chars().count();
                let before: String = buffer.chars().take(prefix_len).collect();
                let after: String = buffer.chars().skip(after_offset).collect();
                *buffer = format!("{}{}{}", before, name, after).into();
                *cursor = prefix_len + name.len();
            }
            picker.deactivate();
            true
        }
        KeyCode::Esc => {
            let after_offset = prefix_len + picker.query.chars().count();
            if buffer.len() >= after_offset {
                let before: String = buffer.chars().take(prefix_len).collect();
                let after: String = buffer.chars().skip(after_offset).collect();
                *buffer = format!("{}{}", before, after).into();
                *cursor = prefix_len;
            }
            picker.deactivate();
            true
        }
        _ => false,
    }
}
