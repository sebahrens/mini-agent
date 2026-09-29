use crossterm::event::KeyEvent;

use crate::ui::pickers::bang;
use crate::ui::pickers::file::FilePicker;
use crate::ui::pickers::handlers;
use crate::ui::pickers::list::ListPicker;
use crate::ui::pickers::models::ModelsPicker;
use crate::ui::pickers::rewind::RewindPicker;

pub enum Picker {
    File(FilePicker),
    Command(ListPicker),
    Prefixed(ListPicker, &'static str),
    Models(ModelsPicker),
    Rewind(RewindPicker),
    /// `!` at the start of the input: previously run shell commands.
    Bang(ListPicker),
}

impl Picker {
    pub fn active(&self) -> bool {
        match self {
            Picker::File(p) => p.active,
            Picker::Command(p) => p.active,
            Picker::Prefixed(p, _) => p.active,
            Picker::Models(p) => p.active,
            Picker::Rewind(p) => p.active(),
            Picker::Bang(p) => p.active,
        }
    }

    /// Whether the picker is still filling in the background (the file
    /// picker's directory walk), so the UI should keep repainting it.
    pub fn is_loading(&self) -> bool {
        matches!(self, Picker::File(p) if p.active && p.is_loading())
    }

    /// Take background results that arrived since the last call. Returns true
    /// when the picker changed and should be repainted.
    pub fn poll_background(&mut self) -> bool {
        match self {
            Picker::File(p) if p.active => p.try_finish_loading(),
            _ => false,
        }
    }

    pub fn set_monochrome(&mut self, monochrome: bool) {
        match self {
            Picker::File(p) => p.set_monochrome(monochrome),
            Picker::Command(p) => p.set_monochrome(monochrome),
            Picker::Prefixed(p, _) => p.set_monochrome(monochrome),
            Picker::Models(p) => p.set_monochrome(monochrome),
            Picker::Rewind(p) => p.set_monochrome(monochrome),
            Picker::Bang(p) => p.set_monochrome(monochrome),
        }
    }

    /// Draw the overlay so it ends just above `floor_row`, the separator over
    /// the input (see `Renderer::picker_floor_row`).
    pub fn draw(&mut self, floor_row: u16) -> std::io::Result<()> {
        match self {
            Picker::File(p) => p.draw(floor_row),
            Picker::Command(p) => p.draw(None, floor_row),
            Picker::Prefixed(p, prefix) => {
                let msg = if *prefix == "/provider " {
                    Some("no matches  (type a registered custom gateway name)")
                } else {
                    None
                };
                p.draw(msg, floor_row)
            }
            Picker::Models(p) => p.draw(floor_row),
            Picker::Rewind(p) => p.draw(floor_row),
            // A new command has no history match; stay out of the way
            // instead of announcing "no matches" for every keystroke.
            Picker::Bang(p) if p.matches.is_empty() => Ok(()),
            Picker::Bang(p) => p.draw(None, floor_row),
        }
    }
}

use super::InputEditor;

/// Keys that move the editor caret or delete at it without going through a
/// query picker. The query pickers mirror their query into the buffer at a
/// position derived from the query itself, so a caret moved behind their back
/// desynchronises the two (and a later Delete could land inside a multi-byte
/// character). These keys therefore close the picker before the editor acts.
pub(crate) fn is_caret_editing_key(key: KeyEvent) -> bool {
    use crossterm::event::KeyCode;
    matches!(
        key.code,
        KeyCode::Left | KeyCode::Right | KeyCode::Delete | KeyCode::Home | KeyCode::End
    )
}

impl InputEditor {
    /// Close an open query picker (everything but the rewind picker, which
    /// does not mirror a query into the buffer). The typed text stays.
    pub(crate) fn close_query_picker(&mut self) {
        if self
            .picker
            .as_ref()
            .is_some_and(|p| p.active() && !matches!(p, Picker::Rewind(_)))
        {
            self.picker = None;
        }
    }

    pub fn handle_picker_key(&mut self, key: KeyEvent) -> bool {
        if is_caret_editing_key(key) && !matches!(self.picker, Some(Picker::Rewind(_))) {
            // Leave the key unconsumed so the editor moves the caret or
            // deletes in plain-text mode.
            self.close_query_picker();
            return false;
        }
        let handled = match self.picker.as_mut() {
            Some(Picker::File(p)) => {
                handlers::handle_file_key(&mut self.buffer, &mut self.cursor, p, key)
            }
            Some(Picker::Command(p)) => {
                let ctx = handlers::CommandPickerCtx {
                    prompt_names: &self.prompt_names,
                    agent_names: &self.agent_names,
                    theme_names: &self.theme_names,
                    quick_model_names: &self.quick_model_names,
                    live_model_names: &self.live_model_names,
                    provider_names: &self.provider_names,
                    security_mode: self
                        .permission
                        .as_ref()
                        .map(|p| p.lock().unwrap_or_else(|e| e.into_inner()).mode()),
                };
                let (handled, replacement) =
                    handlers::handle_command_key(&mut self.buffer, &mut self.cursor, &ctx, p, key);
                if let Some(new) = replacement {
                    self.picker = Some(new);
                }
                handled
            }
            Some(Picker::Prefixed(p, prefix)) => {
                handlers::handle_prefixed_key(&mut self.buffer, &mut self.cursor, p, prefix, key)
            }
            Some(Picker::Models(p)) => {
                handlers::handle_models_key(&mut self.buffer, &mut self.cursor, p, key)
            }
            Some(Picker::Rewind(p)) => p.handle(key),
            Some(Picker::Bang(p)) => {
                bang::handle_bang_key(&mut self.buffer, &mut self.cursor, p, key)
            }
            None => false,
        };
        if handled {
            self.yank_pos = None;
        }
        handled
    }
}
