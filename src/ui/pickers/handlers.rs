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
/// Every offset here is a byte offset: `at` comes from [`mention_start`], and
/// `query_len` is `picker.query.len()` (bytes). Slicing by chars with these
/// values corrupted any buffer containing multi-byte text before the `@`.
fn replace_at_span(
    buffer: &mut CompactString,
    at: usize,
    query_len: usize,
    marker_len: usize,
    replacement: &str,
) -> usize {
    let before = &buffer[..at];
    let after = buffer.get(at + marker_len + query_len..).unwrap_or("");
    let mut next = String::with_capacity(before.len() + replacement.len() + after.len());
    next.push_str(before);
    next.push_str(replacement);
    next.push_str(after);
    *buffer = next.into();
    at + replacement.len()
}

/// Byte offset of the `@` that opened the file picker. The picker mirrors its
/// query into the buffer right before the caret, so the mention being
/// completed is `@<query>` ending at `cursor`; searching the whole buffer for
/// the last `@` would edit a later mention instead.
fn mention_start(buffer: &str, cursor: usize, query: &str) -> Option<usize> {
    let cursor = cursor.min(buffer.len());
    let exact = cursor.checked_sub(query.len() + 1).filter(|&at| {
        buffer
            .get(at..cursor)
            .is_some_and(|span| span.strip_prefix('@') == Some(query))
    });
    exact.or_else(|| buffer.get(..cursor)?.rfind('@'))
}

fn completion_start(buffer: &str, cursor: usize, picker: &FilePicker) -> Option<usize> {
    if let Some(start) = picker.argument_start {
        (buffer.get(start..cursor) == Some(picker.query.as_str())).then_some(start)
    } else {
        mention_start(buffer, cursor, &picker.query)
    }
}

/// Literal quoting for the existing /add parser, never a shell expression.
fn quote_add_path(path: &str) -> Option<String> {
    if path.chars().any(char::is_control) {
        return None;
    }
    if cfg!(windows) {
        if path.contains('"') {
            return None;
        }
        Some(format!("\"{path}\""))
    } else {
        Some(format!(
            "\"{}\"",
            path.replace('\\', "\\\\").replace('"', "\\\"")
        ))
    }
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
                if let Some(at) = completion_start(buffer, *cursor, picker) {
                    *cursor = replace_at_span(
                        buffer,
                        at,
                        0,
                        usize::from(picker.argument_start.is_none()),
                        "",
                    );
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
                if let Some(at) = completion_start(buffer, *cursor, picker) {
                    *cursor = replace_at_span(
                        buffer,
                        at,
                        0,
                        usize::from(picker.argument_start.is_none()),
                        "",
                    );
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
                let path_str = if picker.argument_start.is_some() {
                    let Some(path) = path.to_str().and_then(quote_add_path) else {
                        return true;
                    };
                    format!("{path} ")
                } else if picker.selected_is_mention() {
                    // A mention keeps its own `@` and ends the token so the
                    // prompt text can follow directly.
                    format!("{} ", path.to_string_lossy())
                } else {
                    path.to_string_lossy().to_string()
                };
                if let Some(at) = completion_start(buffer, *cursor, picker) {
                    *cursor = replace_at_span(
                        buffer,
                        at,
                        picker.query.len(),
                        usize::from(picker.argument_start.is_none()),
                        &path_str,
                    );
                }
            }
            picker.deactivate();
            true
        }
        KeyCode::Esc => {
            if picker.argument_start.is_some() {
                picker.deactivate();
                return true;
            }
            if let Some(at) = completion_start(buffer, *cursor, picker) {
                *cursor = replace_at_span(
                    buffer,
                    at,
                    picker.query.len(),
                    usize::from(picker.argument_start.is_none()),
                    "",
                );
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
    /// The session's current model, marked in the `/model` picker.
    pub current_model: Option<String>,
    pub file_workspace: Option<std::sync::Arc<crate::paths::WorkspaceBinding>>,
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
        "/model" | "/models" => {
            !(ctx.quick_model_names.is_empty() && ctx.live_model_names.is_empty())
        }
        "/theme" => !ctx.theme_names.is_empty(),
        "/provider" => !ctx.provider_names.is_empty(),
        "/queue" | "/add" => true,
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
                "/model" | "/models" => {
                    let prefix = if selected == "/model" {
                        "/model "
                    } else {
                        "/models "
                    };
                    let mut mp = ModelsPicker::for_command(prefix, ctx.current_model.clone());
                    mp.set_groups(
                        ctx.quick_model_names.to_vec(),
                        ctx.live_model_names.to_vec(),
                    );
                    mp.activate();
                    Picker::Models(mp)
                }
                "/add" => {
                    let mut files = FilePicker::new();
                    files.activate_for_add(ctx.file_workspace.clone());
                    files.set_add_argument(new_cursor, "");
                    Picker::File(files)
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
                    if other == "/agent" {
                        // `/agent` changes the main agent; `@agent:` starts a
                        // read-only child. Say which one this list is.
                        lp.set_described_items(
                            items
                                .into_iter()
                                .map(|name| (name, "main-agent persona".to_string()))
                                .collect(),
                        );
                    } else {
                        lp.set_items(items);
                    }
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

/// The command the input names in full, when the whole buffer is `/<query>`
/// with the caret at its end and `/<query>` is a listed command or a hidden
/// alias.
fn typed_command(buffer: &str, cursor: usize, picker: &ListPicker) -> Option<String> {
    if cursor != buffer.len() || buffer.strip_prefix('/') != Some(picker.query.as_str()) {
        return None;
    }
    (picker.matches.iter().any(|m| m == buffer) || super::list::is_command_alias(buffer))
        .then(|| buffer.to_string())
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
        // A space after a command typed in full ends the command word: open
        // its argument picker (`/model ` -> model picker), or close
        // completion and let the editor insert the space.
        KeyCode::Char(' ') if typed_command(buffer, *cursor, picker).is_some() => {
            let command = typed_command(buffer, *cursor, picker).unwrap_or_default();
            if opens_sub_picker(&command, ctx)
                && let Some(index) = picker.matches.iter().position(|m| *m == command)
            {
                picker.selected = index;
                return accept_command(buffer, cursor, ctx, picker);
            }
            picker.deactivate();
            (false, None)
        }
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
            // A hidden alias typed in full runs as typed, too.
            let alias_in_full = buffer.strip_prefix('/') == Some(picker.query.as_str())
                && super::list::is_command_alias(buffer.as_str());
            let typed_in_full = typed_in_full || alias_in_full;
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

/// A complete, explicitly typed argument should reach the slash handler on
/// Enter. An unmatched argument may be a raw model ID or a handler subcommand
/// (e.g. `/models refresh`); the handler owns its validation.
fn typed_argument(
    buffer: &str,
    cursor: usize,
    prefix: &str,
    query: &str,
    selected: Option<&str>,
) -> bool {
    !query.is_empty()
        && cursor == buffer.len()
        && buffer.strip_prefix(prefix) == Some(query)
        && selected.is_none_or(|name| name == query)
}

/// How Tab behaves in a query picker: the slash/list pickers insert the
/// highlighted item, the models picker flips between its two groups.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TabBehaviour {
    Select,
    ToggleGroup,
}

/// The operations [`handle_query_key`] needs from a picker that mirrors a
/// typed query into the input buffer after a fixed prefix.
trait QueryPicker {
    fn query(&self) -> &str;
    fn query_cursor(&self) -> usize;
    fn char_input(&mut self, c: char);
    fn backspace(&mut self);
    fn select_prev(&mut self);
    fn select_next(&mut self);
    fn toggle_group(&mut self);
    fn selected_name(&self) -> Option<&str>;
    fn deactivate(&mut self);
    fn has_matches(&self) -> bool;
}

impl QueryPicker for ListPicker {
    fn query(&self) -> &str {
        &self.query
    }
    fn query_cursor(&self) -> usize {
        self.cursor
    }
    fn char_input(&mut self, c: char) {
        ListPicker::char_input(self, c)
    }
    fn backspace(&mut self) {
        ListPicker::backspace(self)
    }
    fn select_prev(&mut self) {
        ListPicker::select_prev(self)
    }
    fn select_next(&mut self) {
        ListPicker::select_next(self)
    }
    fn toggle_group(&mut self) {}
    fn selected_name(&self) -> Option<&str> {
        ListPicker::selected_name(self)
    }
    fn deactivate(&mut self) {
        ListPicker::deactivate(self)
    }
    fn has_matches(&self) -> bool {
        !self.matches.is_empty()
    }
}

impl QueryPicker for ModelsPicker {
    fn query(&self) -> &str {
        &self.query
    }
    fn query_cursor(&self) -> usize {
        self.cursor
    }
    fn char_input(&mut self, c: char) {
        ModelsPicker::char_input(self, c)
    }
    fn backspace(&mut self) {
        ModelsPicker::backspace(self)
    }
    fn select_prev(&mut self) {
        ModelsPicker::select_prev(self)
    }
    fn select_next(&mut self) {
        ModelsPicker::select_next(self)
    }
    fn toggle_group(&mut self) {
        ModelsPicker::toggle_group(self)
    }
    fn selected_name(&self) -> Option<&str> {
        ModelsPicker::selected_name(self)
    }
    fn deactivate(&mut self) {
        ModelsPicker::deactivate(self)
    }
    fn has_matches(&self) -> bool {
        !self.matches.is_empty()
    }
}

/// Replace the `query_len`-byte query that follows the `prefix_len`-byte
/// prefix with `replacement` and return the byte cursor after the inserted
/// text. One allocation; every offset is a byte offset.
fn replace_query_span(
    buffer: &mut CompactString,
    prefix_len: usize,
    query_len: usize,
    replacement: &str,
) -> usize {
    let before = buffer.get(..prefix_len).unwrap_or("");
    let after = buffer.get(prefix_len + query_len..).unwrap_or("");
    let mut next = String::with_capacity(before.len() + replacement.len() + after.len());
    next.push_str(before);
    next.push_str(replacement);
    next.push_str(after);
    *buffer = next.into();
    prefix_len + replacement.len()
}

/// Remove the query span (when the buffer still holds it) and close the picker.
fn cancel_query<P: QueryPicker>(
    buffer: &mut CompactString,
    cursor: &mut usize,
    picker: &mut P,
    prefix_len: usize,
) {
    let query_len = picker.query().len();
    if buffer.len() >= prefix_len + query_len {
        *cursor = replace_query_span(buffer, prefix_len, query_len, "");
    }
    picker.deactivate();
}

/// Backspace: delete the char before the query caret, or, on an empty query,
/// drop the trigger prefix span and close the picker.
fn query_backspace<P: QueryPicker>(
    buffer: &mut CompactString,
    cursor: &mut usize,
    picker: &mut P,
    prefix_len: usize,
) {
    if picker.query_cursor() > 0 {
        picker.backspace();
        let byte_in_query = picker
            .query()
            .char_indices()
            .nth(picker.query_cursor())
            .map(|(i, _)| i)
            .unwrap_or(picker.query().len());
        let remove_pos = prefix_len + byte_in_query;
        if remove_pos < buffer.len() {
            buffer.remove(remove_pos);
        }
        *cursor = prev_char_boundary(buffer, *cursor);
    } else {
        cancel_query(buffer, cursor, picker, prefix_len);
    }
}

fn handle_query_key<P: QueryPicker>(
    buffer: &mut CompactString,
    cursor: &mut usize,
    picker: &mut P,
    prefix: &str,
    key: KeyEvent,
    tab: TabBehaviour,
) -> bool {
    let prefix_len = prefix.len();
    match key.code {
        KeyCode::Enter
            if typed_argument(
                buffer,
                *cursor,
                prefix,
                picker.query(),
                picker.selected_name(),
            ) =>
        {
            picker.deactivate();
            false
        }

        KeyCode::Char(c)
            if c == '\x08' || (c == 'h' && key.modifiers.contains(KeyModifiers::CONTROL)) =>
        {
            query_backspace(buffer, cursor, picker, prefix_len);
            true
        }
        _ if is_ctrl_w(key) => {
            if picker.query_cursor() == 0 {
                return handle_query_key(buffer, cursor, picker, prefix, BACKSPACE, tab);
            }
            while picker.query_cursor() > 0 && !picker.query().is_empty() {
                handle_query_key(buffer, cursor, picker, prefix, BACKSPACE, tab);
            }
            true
        }
        KeyCode::Char(_) if is_modifier_chord(key) => true,
        KeyCode::Char(c) => {
            picker.char_input(c);
            let byte_in_query = picker
                .query()
                .char_indices()
                .nth(picker.query_cursor().saturating_sub(1))
                .map(|(i, _)| i)
                .unwrap_or(picker.query().len());
            buffer.insert(prefix_len + byte_in_query, c);
            *cursor += c.len_utf8();
            true
        }
        KeyCode::Backspace => {
            query_backspace(buffer, cursor, picker, prefix_len);
            true
        }
        KeyCode::Tab
            if tab == TabBehaviour::Select && key.modifiers.contains(KeyModifiers::SHIFT) =>
        {
            picker.select_prev();
            true
        }
        KeyCode::BackTab if tab == TabBehaviour::Select => {
            picker.select_prev();
            true
        }
        KeyCode::Tab | KeyCode::BackTab if tab == TabBehaviour::ToggleGroup => {
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
        KeyCode::Tab if !picker.has_matches() => true,
        KeyCode::Enter | KeyCode::Tab => {
            if let Some(name) = picker.selected_name() {
                let name = name.to_owned();
                let query_len = picker.query().len();
                *cursor = replace_query_span(buffer, prefix_len, query_len, &name);
            }
            picker.deactivate();
            true
        }
        KeyCode::Esc => {
            cancel_query(buffer, cursor, picker, prefix_len);
            true
        }
        _ => false,
    }
}

pub fn handle_prefixed_key(
    buffer: &mut CompactString,
    cursor: &mut usize,
    picker: &mut ListPicker,
    prefix: &str,
    key: KeyEvent,
) -> bool {
    handle_query_key(buffer, cursor, picker, prefix, key, TabBehaviour::Select)
}

pub fn handle_models_key(
    buffer: &mut CompactString,
    cursor: &mut usize,
    picker: &mut ModelsPicker,
    key: KeyEvent,
) -> bool {
    let prefix = picker.prefix;
    handle_query_key(
        buffer,
        cursor,
        picker,
        prefix,
        key,
        TabBehaviour::ToggleGroup,
    )
}

#[cfg(test)]
mod busy67_file_argument_tests {
    use super::*;
    #[test]
    fn literal_path_quoting_roundtrips_without_shell_or_environment_expansion() {
        for path in [
            "雪 space.txt",
            "dollar$HOME@file;touch",
            "quote'file.txt",
            "back\\slash.txt",
        ] {
            let quoted = quote_add_path(path).unwrap();
            assert_eq!(crate::ui::slash::split_args(&quoted).unwrap(), vec![path]);
        }
        #[cfg(not(windows))]
        {
            let path = "both\"and'quotes.txt";
            assert_eq!(
                crate::ui::slash::split_args(&quote_add_path(path).unwrap()).unwrap(),
                vec![path]
            );
        }
        assert!(quote_add_path("control\x1b.txt").is_none());
    }
}

#[cfg(test)]
mod query_picker_parity_tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl_w() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)
    }

    /// Drive both pickers through the same keys with a multi-byte query and
    /// assert the buffer and cursor evolve identically at every step.
    #[test]
    fn list_and_models_pickers_edit_the_buffer_identically() {
        let items = vec!["héllo".to_string(), "héllo-two".to_string()];
        let script = [
            key(KeyCode::Char('h')),
            key(KeyCode::Char('é')),
            key(KeyCode::Char('雪')),
            key(KeyCode::Backspace),
            key(KeyCode::Char('l')),
            ctrl_w(),
            key(KeyCode::Char('h')),
            key(KeyCode::Char('é')),
            key(KeyCode::Esc),
        ];
        for tail in ["", " trailing 雪"] {
            let mut list_buf = CompactString::from(format!("/m {tail}"));
            let mut models_buf = list_buf.clone();
            // Cursor sits right after the prefix, before the tail.
            let mut list_cur = 3;
            let mut models_cur = 3;
            let mut list = ListPicker::new();
            list.set_items(items.clone());
            list.activate();
            let mut models = ModelsPicker::for_command("/m ", None);
            models.set_groups(items.clone(), items.clone());
            models.activate();
            for k in script {
                let a = handle_prefixed_key(&mut list_buf, &mut list_cur, &mut list, "/m ", k);
                let b = handle_models_key(&mut models_buf, &mut models_cur, &mut models, k);
                assert_eq!(a, b, "handled differs for {k:?}");
                assert_eq!(list_buf, models_buf, "buffer differs after {k:?}");
                assert_eq!(list_cur, models_cur, "cursor differs after {k:?}");
                assert_eq!(list.query, models.query);
            }
            assert_eq!(list_buf.as_str(), format!("/m {tail}"));
            assert!(!list.active && !models.active);
        }
    }

    #[test]
    fn enter_inserts_selection_in_place_of_a_multibyte_query() {
        let mut buf = CompactString::from("/m ");
        let mut cur = 3;
        let mut list = ListPicker::new();
        list.set_items(vec!["héllo".to_string()]);
        list.activate();
        for c in "hé".chars() {
            handle_prefixed_key(&mut buf, &mut cur, &mut list, "/m ", key(KeyCode::Char(c)));
        }
        assert!(handle_prefixed_key(
            &mut buf,
            &mut cur,
            &mut list,
            "/m ",
            key(KeyCode::Tab)
        ));
        assert_eq!(buf.as_str(), "/m héllo");
        assert_eq!(cur, buf.len());
    }
}
