pub(crate) mod cursor;
mod pickers;

pub use cursor::cursor_to_line_col;
pub use cursor::{
    count_lines, line_col_to_cursor, line_end, line_start, next_char_boundary, prev_char_boundary,
};
pub use pickers::Picker;

#[cfg(not(windows))]
use crate::process_creation::StdCommandCreationExt;
use crate::ui::pickers::file::FilePicker;
use crate::ui::pickers::list::ListPicker;
use crate::ui::pickers::models::ModelsPicker;
use crate::ui::pickers::rewind::{RewindOutcome, RewindPicker};
use crate::ui::terminal::TerminalGuard;
use compact_str::CompactString;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[cfg(not(windows))]
const MAX_EDITOR_BYTES: u64 = 4 * 1024 * 1024;

#[cfg(not(windows))]
const MAX_EDITOR_ARTIFACTS: usize = 128;

#[cfg(not(windows))]
struct EditorTemp {
    path: std::path::PathBuf,
    directory: std::path::PathBuf,
    handle: cap_std::fs::Dir,
    identity: crate::fs::CheckedMetadata,
    cleanup_attempted: bool,
}

#[cfg(not(windows))]
#[derive(Debug)]
struct EditorFilesRetained {
    directory: std::path::PathBuf,
    directory_is_current: bool,
}

#[cfg(not(windows))]
impl std::fmt::Display for EditorFilesRetained {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.directory_is_current {
            write!(
                f,
                "editor files retained for recovery in {}",
                self.directory.display()
            )
        } else {
            write!(
                f,
                "editor directory changed; files left untouched (original path: {})",
                self.directory.display()
            )
        }
    }
}

#[cfg(not(windows))]
impl std::error::Error for EditorFilesRetained {}

#[cfg(not(windows))]
impl EditorTemp {
    fn create(contents: &[u8]) -> std::io::Result<Self> {
        if contents.len() as u64 > MAX_EDITOR_BYTES {
            return Err(editor_draft_too_large());
        }
        let directory =
            std::env::temp_dir().join(format!("zerostack-editor-{}", uuid::Uuid::new_v4()));
        crate::fs::ensure_private_directory(&directory)?;
        let path = directory.join("message.md");
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options.open(&directory)?;
        let identity = crate::fs::checked_file_metadata(&file)?;
        let temp = Self {
            path,
            directory,
            handle: cap_std::fs::Dir::from_std_file(file),
            identity,
            cleanup_attempted: false,
        };
        temp.validate_directory()?;
        let mut options = cap_std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write;
        temp.handle
            .open_with("message.md", &options)?
            .write_all(contents)?;
        Ok(temp)
    }

    fn validate_directory(&self) -> std::io::Result<()> {
        crate::fs::ensure_same_file(
            &self.directory,
            &self.identity,
            &crate::fs::checked_path_metadata(&self.directory)?,
        )
    }

    fn read_contents(&self) -> std::io::Result<String> {
        self.validate_directory()?;
        let not_regular = || {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "edited draft must be a regular file, not a symlink or special file",
            )
        };
        if !self.handle.symlink_metadata("message.md")?.is_file() {
            return Err(not_regular());
        }
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            // A regular file may be replaced between metadata and open.
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
        }
        let file = self.handle.open_with("message.md", &options)?.into_std();
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(not_regular());
        }
        if metadata.len() > MAX_EDITOR_BYTES {
            return Err(editor_draft_too_large());
        }
        let contents = read_editor_text(file)?;
        self.validate_directory()?;
        Ok(contents)
    }

    fn retain(&mut self) -> EditorFilesRetained {
        self.cleanup_attempted = true;
        EditorFilesRetained {
            directory: self.directory.clone(),
            directory_is_current: self.validate_directory().is_ok(),
        }
    }

    fn cleanup(&mut self) -> std::io::Result<()> {
        // One attempt only: a reported failure must not trigger a second,
        // unreported deletion when Drop runs.
        self.cleanup_attempted = true;
        self.validate_directory()?;
        let mut names = Vec::new();
        // Preflight the complete bounded set before removing any recovery data.
        // Never descend into editor-created directories or follow backup links.
        for entry in self.handle.entries()?.take(MAX_EDITOR_ARTIFACTS + 1) {
            let entry = entry?;
            if names.len() == MAX_EDITOR_ARTIFACTS || entry.file_type()?.is_dir() {
                return Err(std::io::Error::other(
                    "editor cleanup requires at most 128 artifacts and no subdirectories",
                ));
            }
            names.push(entry.file_name());
        }
        self.validate_directory()?;
        for name in names {
            self.handle.remove_file(name)?;
        }
        self.validate_directory()?;
        std::fs::remove_dir(&self.directory)
    }
}

#[cfg(not(windows))]
impl Drop for EditorTemp {
    fn drop(&mut self) {
        if !self.cleanup_attempted
            && let Err(error) = self.cleanup()
        {
            tracing::warn!(directory = %self.directory.display(), %error, "editor cleanup incomplete");
        }
    }
}

#[cfg(not(windows))]
fn editor_draft_too_large() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("editor draft exceeds the {MAX_EDITOR_BYTES}-byte limit"),
    )
}

/// Bound bytes consumed even when the file grows after its metadata check.
#[cfg(not(windows))]
fn read_editor_text(reader: impl std::io::Read) -> std::io::Result<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    reader.take(MAX_EDITOR_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_EDITOR_BYTES {
        return Err(editor_draft_too_large());
    }
    String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

const MAX_KILL_RING: usize = 30;
const MAX_PICKER_PASTE_CHARS: usize = 256;

pub struct InputEditor {
    pub buffer: CompactString,
    pub cursor: usize,
    history: Vec<CompactString>,
    history_pos: Option<usize>,
    draft: Option<CompactString>,
    pub picker: Option<Picker>,
    monochrome: bool,
    prompt_names: Vec<String>,
    agent_names: Vec<String>,
    theme_names: Vec<String>,
    quick_model_names: Vec<String>,
    live_model_names: Vec<String>,
    provider_names: Vec<String>,
    editor: Option<String>,
    kill_ring: Vec<CompactString>,
    yank_pos: Option<usize>,
    yank_len: usize,
}

impl InputEditor {
    pub fn new() -> Self {
        InputEditor {
            buffer: CompactString::new(""),
            cursor: 0,
            history: Vec::new(),
            history_pos: None,
            draft: None,
            picker: None,
            monochrome: false,
            prompt_names: Vec::new(),
            agent_names: Vec::new(),
            theme_names: Vec::new(),
            quick_model_names: Vec::new(),
            live_model_names: Vec::new(),
            provider_names: Vec::new(),
            editor: None,
            kill_ring: Vec::with_capacity(MAX_KILL_RING),
            yank_pos: None,
            yank_len: 0,
        }
    }

    /// Move the cursor to `pos` (a byte offset), clamped to a char boundary
    /// within the buffer. Used when a mouse click places the cursor.
    pub fn set_cursor(&mut self, pos: usize) {
        let pos = pos.min(self.buffer.len());
        self.cursor = if self.buffer.is_char_boundary(pos) {
            pos
        } else {
            prev_char_boundary(&self.buffer, pos)
        };
        self.yank_pos = None;
    }

    pub fn clear_buffer(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.history_pos = None;
        self.draft = None;
        self.yank_pos = None;
    }

    /// Replace the input buffer with `text`, cursor at the end. Used by the
    /// rewind flow to drop the chosen user turn back into the box for editing.
    pub fn load_text(&mut self, text: &str) {
        self.buffer = CompactString::new(text);
        // `cursor` is a byte offset into `buffer` (sliced as `buffer[..cursor]`
        // and advanced by `len_utf8()` elsewhere), so end-of-buffer is the byte
        // length, not the char count — they differ for multi-byte (e.g. CJK) text.
        self.cursor = self.buffer.len();
        self.history_pos = None;
        self.draft = None;
        self.yank_pos = None;
    }

    pub fn set_quick_model_names(&mut self, names: Vec<String>) {
        self.quick_model_names = names;
    }

    pub fn set_live_model_names(&mut self, names: Vec<String>) {
        self.live_model_names = names;
    }

    pub fn set_provider_names(&mut self, names: Vec<String>) {
        self.provider_names = names;
    }

    pub fn set_editor(&mut self, editor: String) {
        self.editor = Some(editor);
    }

    pub fn set_monochrome(&mut self, monochrome: bool) {
        self.monochrome = monochrome;
        if let Some(ref mut picker) = self.picker {
            picker.set_monochrome(monochrome);
        }
    }

    pub fn set_prompt_names(&mut self, names: Vec<String>) {
        self.prompt_names = names;
    }

    pub fn set_agent_names(&mut self, names: Vec<String>) {
        self.agent_names = names;
    }

    pub fn set_theme_names(&mut self, names: Vec<String>) {
        self.theme_names = names;
    }

    pub fn load_global_history(&mut self) {
        if let Ok(entries) = crate::session::chat_history::load_history() {
            self.history = entries
                .into_iter()
                .map(|e| CompactString::new(e.content))
                .collect();
            self.history_pos = None;
        }
    }

    pub fn start_file_picker(&mut self) {
        let mut picker = FilePicker::new();
        picker.set_monochrome(self.monochrome);
        picker.activate();
        self.picker = Some(Picker::File(picker));
    }

    pub fn start_command_picker(&mut self) {
        let mut picker = ListPicker::with_static_commands();
        picker.set_monochrome(self.monochrome);
        picker.activate();
        self.picker = Some(Picker::Command(picker));
    }

    pub fn start_models_picker(&mut self) {
        let mut picker = ModelsPicker::new();
        picker.set_monochrome(self.monochrome);
        picker.set_groups(
            self.quick_model_names.clone(),
            self.live_model_names.clone(),
        );
        picker.activate();
        self.picker = Some(Picker::Models(picker));
    }

    pub fn start_provider_picker(&mut self) {
        let mut picker = ListPicker::new();
        picker.set_monochrome(self.monochrome);
        if !self.provider_names.is_empty() {
            picker.set_items(self.provider_names.clone());
        }
        picker.activate();
        self.picker = Some(Picker::Prefixed(picker, "/provider "));
    }

    /// Open the double-Esc rewind picker over the given `(message_index,
    /// preview)` user turns. No-op when there is nothing to rewind to.
    pub fn start_rewind_picker(&mut self, targets: Vec<(usize, String)>) {
        if targets.is_empty() {
            return;
        }
        let mut picker = RewindPicker::new(targets);
        picker.set_monochrome(self.monochrome);
        picker.activate();
        self.picker = Some(Picker::Rewind(picker));
        self.history_pos = None;
        self.draft = None;
    }

    /// Take the rewind picker's resolved outcome, if it has one. Also clears the
    /// finished picker so the input box returns to normal.
    pub fn take_rewind_outcome(&mut self) -> Option<RewindOutcome> {
        let outcome = match self.picker.as_mut() {
            Some(Picker::Rewind(p)) => p.take_outcome(),
            _ => None,
        };
        if outcome.is_some() {
            self.picker = None;
        }
        outcome
    }

    pub fn start_prompt_picker(&mut self) {
        let mut picker = ListPicker::new();
        picker.set_monochrome(self.monochrome);
        if !self.prompt_names.is_empty() {
            picker.set_items(self.prompt_names.clone());
        }
        picker.activate();
        self.picker = Some(Picker::Prefixed(picker, "/prompt "));
    }

    pub fn start_dot_picker(&mut self) {
        let mut picker = ListPicker::new();
        picker.set_monochrome(self.monochrome);
        if !self.prompt_names.is_empty() {
            picker.set_items(self.prompt_names.clone());
        }
        picker.activate();
        self.picker = Some(Picker::Prefixed(picker, "."));
    }

    pub fn start_theme_picker(&mut self) {
        let mut picker = ListPicker::new();
        picker.set_monochrome(self.monochrome);
        if !self.theme_names.is_empty() {
            picker.set_items(self.theme_names.clone());
        }
        picker.activate();
        self.picker = Some(Picker::Prefixed(picker, "/theme "));
    }

    pub fn open_in_editor(&mut self, terminal_guard: &mut TerminalGuard) -> anyhow::Result<()> {
        #[cfg(windows)]
        {
            let _ = terminal_guard;
            anyhow::bail!(
                "opening $EDITOR is unsupported on Windows because no portable editor-command grammar is configured"
            );
        }

        #[cfg(not(windows))]
        self.open_in_editor_unix(terminal_guard)
    }

    #[cfg(not(windows))]
    fn open_in_editor_unix(&mut self, terminal_guard: &mut TerminalGuard) -> anyhow::Result<()> {
        terminal_guard.suspend()?;
        let result = self.edit_buffer();
        terminal_guard.resume()?;
        result
    }

    #[cfg(not(windows))]
    fn edit_buffer(&mut self) -> anyhow::Result<()> {
        use anyhow::Context;

        let editor = self
            .editor
            .clone()
            .or_else(|| std::env::var("EDITOR").ok())
            .unwrap_or_else(|| "editor".to_string());

        let mut tmp = EditorTemp::create(self.buffer.as_bytes())?;

        let result = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{} \"$1\"", editor))
            .arg("sh")
            .arg(&tmp.path)
            .status_guarded();

        let contents = tmp
            .read_contents()
            .context("could not read edited draft; original input retained");
        if let Ok(content) = &contents
            && content != self.buffer.as_str()
        {
            self.load_text(content.trim_end());
        }
        let command = result
            .context("could not launch the configured editor")
            .and_then(|exit_status| {
                anyhow::ensure!(
                    exit_status.success(),
                    "configured editor exited with {exit_status}"
                );
                Ok(())
            });
        let read_failed = contents.is_err();
        let result = match (command, contents) {
            (Err(error), Err(read_error)) => Err(error.context(format!("{read_error:#}"))),
            (Err(error), _) => Err(error),
            (Ok(()), contents) => contents.map(|_| ()),
        };
        if read_failed {
            return result.context(tmp.retain());
        }
        if let Err(cleanup_error) = tmp.cleanup() {
            return match result {
                Ok(()) => Err(anyhow::Error::from(cleanup_error)),
                Err(error) => Err(error.context(format!("editor cleanup failed: {cleanup_error}"))),
            }
            .context(tmp.retain());
        }
        result
    }

    pub fn handle_paste(&mut self, data: String) {
        // Keep query pickers open only for bounded, single-line text. Larger or
        // control-bearing pastes use the normal atomic buffer path below.
        let picker_accepts_query = self.picker.as_ref().is_some_and(|picker| {
            picker.active()
                && !matches!(picker, Picker::Rewind(_))
                && data.chars().take(MAX_PICKER_PASTE_CHARS + 1).count() <= MAX_PICKER_PASTE_CHARS
                && data.chars().all(|c| !c.is_control())
        });
        if picker_accepts_query {
            for c in data.chars() {
                let handled =
                    self.handle_picker_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
                debug_assert!(handled, "active query picker must accept printable text");
            }
        } else {
            if self.picker.as_ref().is_some_and(Picker::active) {
                self.picker = None;
            }
            self.buffer.insert_str(self.cursor, &data);
            self.cursor += data.len();
        }
        self.history_pos = None;
        self.draft = None;
        self.yank_pos = None;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<CompactString> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        if ctrl {
            match key.code {
                KeyCode::Char('a') => {
                    let current = line_start(&self.buffer, self.cursor);
                    if self.cursor == current {
                        let (line, _) = cursor_to_line_col(&self.buffer, self.cursor);
                        if line > 0 {
                            self.cursor = line_end(&self.buffer, current - 1);
                        }
                    } else {
                        self.cursor = current;
                    }
                    self.yank_pos = None;
                    return None;
                }
                KeyCode::Char('e') => {
                    let current = line_end(&self.buffer, self.cursor);
                    if self.cursor == current {
                        let (line, _) = cursor_to_line_col(&self.buffer, self.cursor);
                        let total = count_lines(&self.buffer);
                        if line + 1 < total {
                            self.cursor = line_start(&self.buffer, self.cursor + 1);
                        }
                    } else {
                        self.cursor = current;
                    }
                    self.yank_pos = None;
                    return None;
                }
                KeyCode::Char('b') => {
                    if self.cursor > 0 {
                        self.cursor = prev_char_boundary(&self.buffer, self.cursor);
                    }
                    self.yank_pos = None;
                    return None;
                }
                KeyCode::Char('f') => {
                    if self.cursor < self.buffer.len() {
                        self.cursor = next_char_boundary(&self.buffer, self.cursor);
                    }
                    self.yank_pos = None;
                    return None;
                }
                KeyCode::Char('p') => {
                    return self.cursor_up();
                }
                KeyCode::Char('n') => {
                    return self.cursor_down();
                }
                KeyCode::Char('w') => {
                    let deleted = self.delete_prev_word();
                    if !deleted.is_empty() {
                        self.push_kill(deleted);
                    }
                    self.yank_pos = None;
                    return None;
                }
                KeyCode::Char('u') => {
                    // `cursor` is a byte offset; slice bytes, never chars.
                    if self.cursor > 0 {
                        let deleted: CompactString = self.buffer[..self.cursor].into();
                        let remaining: CompactString = self.buffer[self.cursor..].into();
                        self.buffer = remaining;
                        self.cursor = 0;
                        self.push_kill(deleted);
                    }
                    self.yank_pos = None;
                    return None;
                }
                KeyCode::Char('k') => {
                    if self.cursor < self.buffer.len() {
                        let deleted: CompactString = self.buffer[self.cursor..].into();
                        self.buffer.truncate(self.cursor);
                        self.push_kill(deleted);
                    }
                    self.yank_pos = None;
                    return None;
                }
                KeyCode::Char('d') => {
                    if self.cursor < self.buffer.len() {
                        self.buffer.remove(self.cursor);
                    }
                    self.yank_pos = None;
                    return None;
                }
                KeyCode::Char('y') => {
                    if self.kill_ring.is_empty() {
                        return None;
                    }
                    let pos = self.yank_pos.unwrap_or(0);
                    let text = &self.kill_ring[pos];
                    self.buffer.insert_str(self.cursor, text);
                    self.yank_len = text.len();
                    self.cursor += text.len();
                    self.yank_pos = Some(pos);
                    return None;
                }
                _ => {}
            }
        }

        if alt {
            match key.code {
                KeyCode::Char('b') => {
                    self.cursor = self.prev_word_start();
                    self.yank_pos = None;
                    return None;
                }
                KeyCode::Char('f') => {
                    self.cursor = self.next_word_end();
                    self.yank_pos = None;
                    return None;
                }
                KeyCode::Char('d') => {
                    let deleted = self.delete_next_word();
                    if !deleted.is_empty() {
                        self.push_kill(deleted);
                    }
                    self.yank_pos = None;
                    return None;
                }
                KeyCode::Char('y') => {
                    if let Some(pos) = self.yank_pos
                        && self.kill_ring.len() > 1
                    {
                        // Byte offsets throughout: `yank_len` is the byte
                        // length of the previously yanked text.
                        let start = self.cursor.saturating_sub(self.yank_len);
                        if start <= self.cursor
                            && self.buffer.is_char_boundary(start)
                            && self.buffer.is_char_boundary(self.cursor)
                        {
                            let before = &self.buffer[..start];
                            let after = &self.buffer[self.cursor..];
                            let mut new_buf = String::with_capacity(before.len() + after.len());
                            new_buf.push_str(before);
                            new_buf.push_str(after);
                            self.buffer = CompactString::new(&new_buf);
                            self.cursor = start;
                        }
                        let new_pos = if pos == 0 {
                            self.kill_ring.len() - 1
                        } else {
                            pos - 1
                        };
                        self.yank_pos = Some(new_pos);
                        let text = &self.kill_ring[new_pos];
                        self.buffer.insert_str(self.cursor, text);
                        self.yank_len = text.len();
                        self.cursor += text.len();
                    }
                    return None;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Enter
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if self.picker.as_ref().is_some_and(|p| p.active()) {
                    return None;
                }
                self.buffer.insert(self.cursor, '\n');
                self.cursor += 1;
                None
            }
            KeyCode::Enter => {
                if self.picker.as_ref().is_some_and(|p| p.active()) {
                    return None;
                }
                let text = self.buffer.clone();
                let is_blank = text.trim().is_empty();
                if !is_blank {
                    self.history.push(text.clone());
                }
                self.history_pos = None;
                self.draft = None;
                self.buffer.clear();
                self.cursor = 0;
                self.yank_pos = None;
                if text.is_empty() { None } else { Some(text) }
            }
            KeyCode::Char(c)
                if c == '\x08' || (c == 'h' && key.modifiers.contains(KeyModifiers::CONTROL)) =>
            {
                if self.cursor > 0 {
                    self.cursor = prev_char_boundary(&self.buffer, self.cursor);
                    self.buffer.remove(self.cursor);
                }
                None
            }
            KeyCode::Char(c) => {
                if c == '@' {
                    let at_word_start = self.cursor == 0
                        || self.buffer[..self.cursor]
                            .chars()
                            .next_back()
                            .is_some_and(|prev| prev == ' ');
                    if at_word_start {
                        self.start_file_picker();
                    }
                }
                if c == '/' && self.cursor == 0 {
                    self.start_command_picker();
                }
                if c == '.' && self.cursor == 0 {
                    self.buffer.insert(self.cursor, c);
                    self.cursor += c.len_utf8();
                    self.start_dot_picker();
                    self.yank_pos = None;
                    return None;
                }
                self.buffer.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                self.history_pos = None;
                self.draft = None;
                self.yank_pos = None;

                if (self.picker.is_none() || !self.picker.as_ref().is_some_and(|p| p.active()))
                    && self.buffer.starts_with("/prompt ")
                {
                    let after_prefix: String = self.buffer.chars().skip("/prompt ".len()).collect();
                    if !after_prefix.is_empty() && c != ' ' {
                        let query_len = after_prefix.len();
                        if query_len == 1 {
                            self.start_prompt_picker();
                            if let Some(Picker::Prefixed(ref mut pp, _)) = self.picker {
                                pp.char_input(c);
                            }
                        }
                    }
                }
                if (self.picker.is_none() || !self.picker.as_ref().is_some_and(|p| p.active()))
                    && self.buffer.starts_with("/models ")
                {
                    let after_prefix: String = self.buffer.chars().skip("/models ".len()).collect();
                    if !after_prefix.is_empty() && c != ' ' {
                        let query_len = after_prefix.len();
                        if query_len == 1 {
                            self.start_models_picker();
                            if let Some(Picker::Models(ref mut mp)) = self.picker {
                                mp.char_input(c);
                            }
                        }
                    }
                }
                if (self.picker.is_none() || !self.picker.as_ref().is_some_and(|p| p.active()))
                    && self.buffer.starts_with("/theme ")
                {
                    let after_prefix: String = self.buffer.chars().skip("/theme ".len()).collect();
                    if !after_prefix.is_empty() && c != ' ' {
                        let query_len = after_prefix.len();
                        if query_len == 1 {
                            self.start_theme_picker();
                            if let Some(Picker::Prefixed(ref mut tp, _)) = self.picker {
                                tp.char_input(c);
                            }
                        }
                    }
                }
                if (self.picker.is_none() || !self.picker.as_ref().is_some_and(|p| p.active()))
                    && self.buffer.starts_with("/provider ")
                {
                    let after_prefix: String =
                        self.buffer.chars().skip("/provider ".len()).collect();
                    if !after_prefix.is_empty() && c != ' ' {
                        let query_len = after_prefix.len();
                        if query_len == 1 {
                            self.start_provider_picker();
                            if let Some(Picker::Prefixed(ref mut pp, _)) = self.picker {
                                pp.char_input(c);
                            }
                        }
                    }
                }

                None
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor = prev_char_boundary(&self.buffer, self.cursor);
                    self.buffer.remove(self.cursor);
                }
                self.yank_pos = None;
                None
            }
            KeyCode::Delete => {
                if self.cursor < self.buffer.len() {
                    self.buffer.remove(self.cursor);
                }
                self.yank_pos = None;
                None
            }
            KeyCode::Left => {
                if self.cursor > 0 {
                    self.cursor = prev_char_boundary(&self.buffer, self.cursor);
                }
                self.yank_pos = None;
                None
            }
            KeyCode::Right => {
                if self.cursor < self.buffer.len() {
                    self.cursor = next_char_boundary(&self.buffer, self.cursor);
                }
                self.yank_pos = None;
                None
            }
            KeyCode::Up => {
                self.yank_pos = None;
                self.cursor_up()
            }
            KeyCode::Down => {
                self.yank_pos = None;
                self.cursor_down()
            }
            KeyCode::Home => {
                self.cursor = 0;
                self.yank_pos = None;
                None
            }
            KeyCode::End => {
                self.cursor = self.buffer.len();
                self.yank_pos = None;
                None
            }
            KeyCode::Tab => {
                self.buffer.insert_str(self.cursor, "  ");
                self.cursor += 2;
                self.yank_pos = None;
                None
            }
            _ => None,
        }
    }

    fn history_up(&mut self) -> Option<CompactString> {
        let hist_len = self.history.len();
        if hist_len == 0 || self.history_pos == Some(0) {
            return None;
        }
        if self.history_pos.is_none() {
            self.draft = Some(self.buffer.clone());
        }
        let pos = match self.history_pos {
            Some(p) if p > 0 => p - 1,
            Some(_) => unreachable!(),
            None => hist_len - 1,
        };
        self.history_pos = Some(pos);
        self.buffer = self.history[pos].clone();
        self.cursor = 0;
        None
    }

    fn history_down(&mut self) -> Option<CompactString> {
        match self.history_pos {
            Some(pos) if pos + 1 < self.history.len() => {
                let new_pos = pos + 1;
                self.history_pos = Some(new_pos);
                self.buffer = self.history[new_pos].clone();
                self.cursor = self.buffer.len();
            }
            Some(_) => {
                self.history_pos = None;
                if let Some(draft) = self.draft.take() {
                    self.buffer = draft.clone();
                    self.cursor = self.buffer.len();
                } else {
                    self.buffer.clear();
                    self.cursor = 0;
                }
            }
            None => {}
        }
        None
    }

    fn cursor_up(&mut self) -> Option<CompactString> {
        let (line, col) = cursor_to_line_col(&self.buffer, self.cursor);
        if line > 0 {
            let line_len =
                line_end(&self.buffer, self.cursor) - line_start(&self.buffer, self.cursor);
            let target = line_col_to_cursor(
                &self.buffer,
                line - 1,
                if col >= line_len { usize::MAX } else { col },
            );
            self.cursor = target;
            None
        } else {
            self.history_up()
        }
    }

    fn cursor_down(&mut self) -> Option<CompactString> {
        let (line, col) = cursor_to_line_col(&self.buffer, self.cursor);
        let total = count_lines(&self.buffer);
        if line + 1 < total {
            let line_len =
                line_end(&self.buffer, self.cursor) - line_start(&self.buffer, self.cursor);
            let target = line_col_to_cursor(
                &self.buffer,
                line + 1,
                if col >= line_len { usize::MAX } else { col },
            );
            self.cursor = target;
            None
        } else {
            self.history_down()
        }
    }

    fn push_kill(&mut self, text: CompactString) {
        if text.is_empty() {
            return;
        }
        if self.kill_ring.first() == Some(&text) {
            return;
        }
        self.kill_ring.insert(0, text);
        if self.kill_ring.len() > MAX_KILL_RING {
            self.kill_ring.pop();
        }
    }

    fn prev_word_start(&self) -> usize {
        if self.cursor == 0 {
            return 0;
        }
        let pairs: Vec<(usize, char)> = self.buffer.char_indices().collect();
        if pairs.is_empty() {
            return 0;
        }
        let char_idx = pairs
            .iter()
            .position(|&(bi, _)| bi >= self.cursor)
            .unwrap_or(pairs.len());
        let mut pos = char_idx;
        while pos > 0 && pairs[pos - 1].1 == ' ' {
            pos -= 1;
        }
        while pos > 0 && pairs[pos - 1].1 != ' ' {
            pos -= 1;
        }
        if pos < pairs.len() {
            pairs[pos].0
        } else {
            self.buffer.len()
        }
    }

    fn next_word_end(&self) -> usize {
        let pairs: Vec<(usize, char)> = self.buffer.char_indices().collect();
        let len = pairs.len();
        if len == 0 {
            return 0;
        }
        let char_idx = pairs
            .iter()
            .position(|&(bi, _)| bi >= self.cursor)
            .unwrap_or(len);
        let mut pos = char_idx;
        while pos < len && pairs[pos].1 == ' ' {
            pos += 1;
        }
        while pos < len && pairs[pos].1 != ' ' {
            pos += 1;
        }
        if pos < len {
            pairs[pos].0
        } else {
            self.buffer.len()
        }
    }

    fn delete_prev_word(&mut self) -> CompactString {
        if self.cursor == 0 || self.buffer.is_empty() {
            return CompactString::new("");
        }
        let start = self.prev_word_start();
        let deleted: CompactString = self.buffer[start..self.cursor].into();
        let before = &self.buffer[..start];
        let after = &self.buffer[self.cursor..];
        let mut new_buf = String::with_capacity(before.len() + after.len());
        new_buf.push_str(before);
        new_buf.push_str(after);
        self.buffer = CompactString::new(&new_buf);
        self.cursor = start;
        deleted
    }

    fn delete_next_word(&mut self) -> CompactString {
        if self.cursor >= self.buffer.len() {
            return CompactString::new("");
        }
        let end = self.next_word_end();
        let deleted: CompactString = self.buffer[self.cursor..end].into();
        let before = &self.buffer[..self.cursor];
        let after = &self.buffer[end..];
        let mut new_buf = String::with_capacity(before.len() + after.len());
        new_buf.push_str(before);
        new_buf.push_str(after);
        self.buffer = CompactString::new(&new_buf);
        deleted
    }
}

#[cfg(all(test, unix))]
mod editor_temp_tests {
    use super::EditorTemp;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn editor_results_preserve_drafts_and_report_launch_exit_and_read_failures() {
        let cases: &[(&str, &str, &[&str])] = &[
            ("true", "original draft  \n", &[]),
            (
                "/nonexistent-mini-agent-editor",
                "original draft  \n",
                &["127"],
            ),
            ("sh -c 'exit 7' sh", "original draft  \n", &["7"]),
            (
                "sh -c 'printf edited > \"$1\"; exit 9' sh",
                "edited",
                &["9"],
            ),
            ("sh -c 'printf edited > \"$1\"' sh", "edited", &[]),
            (
                "sh -c 'printf replaced > \"$1.new\"; mv \"$1.new\" \"$1\"' sh",
                "replaced",
                &[],
            ),
            (
                "sh -c 'rm \"$1\"' sh",
                "original draft  \n",
                &["could not read edited draft", "original input retained"],
            ),
            (
                "sh -c 'rm \"$1\"; exit 7' sh",
                "original draft  \n",
                &["could not read edited draft", "exited with", "7"],
            ),
            (
                "sh -c 'printf \"\\377\" > \"$1\"' sh",
                "original draft  \n",
                &["could not read edited draft", "utf-8"],
            ),
            (
                "sh -c 'rm \"$1\"; mkfifo \"$1\"' sh",
                "original draft  \n",
                &["regular file"],
            ),
            (
                "sh -c 'rm \"$1\"; ln -s /etc/hosts \"$1\"' sh",
                "original draft  \n",
                &["regular file"],
            ),
            (
                "sh -c 'dd if=/dev/zero of=\"$1\" bs=1 count=0 seek=4194305 2>/dev/null' sh",
                "original draft  \n",
                &["4194304-byte limit"],
            ),
            (
                "sh -c 'printf edited > \"$1\"; mkdir \"$1.d\"' sh",
                "edited",
                &["no subdirectories", "retained for recovery"],
            ),
            (
                "sh -c 'printf edited > \"$1\"; mkdir \"$1.d\"; exit 7' sh",
                "edited",
                &[
                    "no subdirectories",
                    "retained for recovery",
                    "exited with",
                    "7",
                ],
            ),
        ];
        for &(command, expected, errors) in cases {
            let record = std::env::temp_dir().join(format!("editor-test-{}", uuid::Uuid::new_v4()));
            let mut input = super::InputEditor::new();
            input.load_text("original draft  \n");
            // Every real editor invocation writes a sibling backup. Record its
            // directory independently so success also proves full cleanup.
            input.set_editor(format!(
                "printf '%s' \"$1\" > '{}'; cp \"$1\" \"$1~\"; {command}",
                record.to_str().unwrap().replace('\'', "'\\''")
            ));
            let result = input.edit_buffer();
            let draft = std::path::PathBuf::from(std::fs::read_to_string(&record).unwrap());
            std::fs::remove_file(record).unwrap();
            let directory = draft.parent().unwrap();
            let retained = result
                .as_ref()
                .err()
                .and_then(|e| e.downcast_ref::<super::EditorFilesRetained>());
            let backup = std::fs::read(draft.with_file_name("message.md~"));
            let directory_exists = directory.exists();
            // Only fixture-owned paths; remove retained data even if a later
            // assertion fails while testing a deliberately broken version.
            if directory_exists {
                std::fs::remove_dir_all(directory).unwrap();
            }
            let should_retain = errors.iter().any(|e| {
                matches!(
                    *e,
                    "could not read edited draft"
                        | "regular file"
                        | "4194304-byte limit"
                        | "retained for recovery"
                )
            });
            assert_eq!(retained.is_some(), should_retain, "{command}: {result:?}");
            assert_eq!(directory_exists, should_retain, "{command}");
            if let Some(retained) = retained {
                assert!(retained.directory_is_current);
                assert_eq!(retained.directory, directory);
                assert_eq!(backup.unwrap(), b"original draft  \n");
            }
            assert_eq!(input.buffer.as_str(), expected, "{command}");
            if errors.is_empty() {
                result.unwrap();
            } else {
                let error = format!("{:#}", result.unwrap_err());
                for expected in errors {
                    assert!(error.contains(expected), "{command}: {error}");
                }
            }
        }
    }

    #[test]
    fn editor_byte_limits_bound_reads_and_reject_oversized_input() {
        use super::{MAX_EDITOR_BYTES, read_editor_text};
        use std::io::Cursor;

        for length in [0, MAX_EDITOR_BYTES, MAX_EDITOR_BYTES + 4096] {
            let mut reader = Cursor::new(vec![b'x'; length as usize]);
            let result = read_editor_text(&mut reader);
            if length <= MAX_EDITOR_BYTES {
                assert_eq!(result.unwrap().len() as u64, length);
            } else {
                assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
            }
            // The content reader must cap consumption independently of the
            // file's earlier metadata: files can grow after that check.
            assert_eq!(reader.position(), length.min(MAX_EDITOR_BYTES + 1));
        }

        let mut input = super::InputEditor::new();
        let original = "x".repeat(MAX_EDITOR_BYTES as usize + 1);
        input.load_text(&original);
        input.set_editor("sh -c 'printf changed > \"$1\"' sh".into());
        let error = input.edit_buffer().unwrap_err();
        assert!(error.to_string().contains("byte limit"));
        assert_eq!(input.buffer.as_str(), original);
    }

    #[test]
    fn editor_temp_is_private_and_removed_on_drop() {
        let target = std::env::temp_dir().join(format!("editor-test-{}", uuid::Uuid::new_v4()));
        std::fs::write(&target, b"foreign data").unwrap();
        let temp = EditorTemp::create(b"secret draft").unwrap();
        let path = temp.path.clone();
        let directory = temp.directory.clone();
        std::fs::write(directory.join("message.md~"), b"editor backup").unwrap();
        std::os::unix::fs::symlink(&target, directory.join("linked-backup")).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        drop(temp);
        let foreign_contents = std::fs::read(&target);
        std::fs::remove_file(target).unwrap();
        assert!(!path.exists());
        assert!(!directory.exists());
        assert_eq!(foreign_contents.unwrap(), b"foreign data");
    }

    #[test]
    fn cleanup_budget_preflights_before_deleting_recovery_data() {
        for count in [super::MAX_EDITOR_ARTIFACTS, super::MAX_EDITOR_ARTIFACTS + 1] {
            let mut temp = EditorTemp::create(b"original draft").unwrap();
            let directory = temp.directory.clone();
            for i in 1..count {
                std::fs::write(directory.join(format!("backup-{i}")), b"backup").unwrap();
            }
            let result = temp.cleanup();
            drop(temp);
            if count == super::MAX_EDITOR_ARTIFACTS {
                result.unwrap();
                assert!(!directory.exists());
            } else {
                let remaining = std::fs::read_dir(&directory).unwrap().count();
                let draft = std::fs::read(directory.join("message.md")).unwrap();
                std::fs::remove_dir_all(directory).unwrap();
                assert!(result.is_err());
                assert_eq!(
                    remaining, count,
                    "preflight refusal must preserve all artifacts"
                );
                assert_eq!(draft, b"original draft");
            }
        }
    }

    #[test]
    fn replaced_editor_root_is_neither_read_nor_cleaned() {
        for symlink in [false, true] {
            let temp = EditorTemp::create(b"owned draft").unwrap();
            let directory = temp.directory.clone();
            let saved = directory.with_extension("saved");
            let foreign = directory.with_extension("foreign");
            std::fs::rename(&directory, &saved).unwrap();
            std::fs::create_dir(&foreign).unwrap();
            std::fs::write(foreign.join("message.md"), b"foreign draft").unwrap();
            if symlink {
                std::os::unix::fs::symlink(&foreign, &directory).unwrap();
            } else {
                std::fs::rename(&foreign, &directory).unwrap();
            }
            let read_result = temp.read_contents();
            drop(temp);
            let foreign_contents = std::fs::read(if symlink {
                foreign.join("message.md")
            } else {
                directory.join("message.md")
            });
            let owned_contents = std::fs::read(saved.join("message.md"));
            // Clean fixture-owned paths before assertions, including on the
            // intentionally broken implementation used by the negative control.
            let _ = std::fs::remove_dir_all(&directory);
            let _ = std::fs::remove_dir_all(&foreign);
            std::fs::remove_dir_all(&saved).unwrap();
            assert!(read_result.is_err(), "replacement must not become input");
            assert_eq!(foreign_contents.unwrap(), b"foreign draft");
            assert_eq!(owned_contents.unwrap(), b"owned draft");
        }
    }
}
