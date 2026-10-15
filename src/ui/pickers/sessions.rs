//! Workspace-scoped session selection. Keys select metadata; loading is owned
//! by the frontend, which revalidates the private record and workspace binding.

use crate::session::catalogue::{Catalogue, ResumeSelection};
use crossterm::cursor::MoveTo;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::{Color, ResetColor, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{Clear, ClearType};
use std::io::{self, Write};
use std::sync::Arc;

pub(crate) enum SessionPick {
    Selected(ResumeSelection),
    Reload(bool),
    Cancelled,
}

pub(crate) struct SessionsPicker {
    pub active: bool,
    pub all: bool,
    pub query: String,
    pub selected: usize,
    pub matches: Vec<usize>,
    pub catalogue: Catalogue,
    pub workspace: Arc<crate::paths::WorkspaceBinding>,
    /// Lowercase filter text per catalogue row, built once in `new`.
    haystacks: Vec<String>,
    monochrome: bool,
    outcome: Option<SessionPick>,
}

impl SessionsPicker {
    pub(crate) fn new(
        catalogue: Catalogue,
        workspace: Arc<crate::paths::WorkspaceBinding>,
        all: bool,
    ) -> Self {
        let haystacks = catalogue
            .rows
            .iter()
            .map(|row| {
                format!(
                    "{} {} {} {} {}",
                    row.title, row.id, row.working_dir, row.provider, row.model
                )
                .to_lowercase()
            })
            .collect();
        let mut picker = Self {
            active: true,
            all,
            query: String::new(),
            selected: 0,
            matches: Vec::new(),
            catalogue,
            workspace,
            haystacks,
            monochrome: false,
            outcome: None,
        };
        picker.filter();
        picker
    }
    pub(crate) fn set_monochrome(&mut self, value: bool) {
        self.monochrome = value;
    }
    pub(crate) fn take_outcome(&mut self) -> Option<SessionPick> {
        self.outcome.take()
    }
    fn filter(&mut self) {
        let query = self.query.to_lowercase();
        let mut ranked: Vec<_> = self
            .haystacks
            .iter()
            .enumerate()
            .filter_map(|(index, text)| {
                super::fuzzy_score_lower(text, &query)
                    .map(|score| (std::cmp::Reverse(score), index))
            })
            .collect();
        ranked.sort();
        self.matches = ranked.into_iter().map(|(_, index)| index).collect();
        self.selected = 0;
    }
    pub(crate) fn paste(&mut self, text: &str) {
        let room = 256_usize.saturating_sub(self.query.chars().count());
        self.query
            .extend(text.chars().filter(|c| !c.is_control()).take(room));
        self.filter();
    }
    pub(crate) fn handle(&mut self, key: KeyEvent) -> bool {
        let plain = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
        if key.code == KeyCode::Esc
            || key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c' | 'd'))
        {
            self.active = false;
            self.outcome = Some(SessionPick::Cancelled);
            return true;
        }
        match key.code {
            KeyCode::Enter if plain => {
                if let Some(index) = self.matches.get(self.selected) {
                    self.outcome = Some(SessionPick::Selected(ResumeSelection {
                        row: self.catalogue.rows[*index].clone(),
                        all: self.all,
                        workspace: self.workspace.clone(),
                    }));
                    self.active = false;
                }
            }
            KeyCode::Tab if plain => {
                self.outcome = Some(SessionPick::Reload(!self.all));
                self.active = false;
            }
            KeyCode::Down if plain && !self.matches.is_empty() => {
                self.selected = (self.selected + 1) % self.matches.len();
            }
            KeyCode::Up | KeyCode::BackTab if plain && !self.matches.is_empty() => {
                self.selected = (self.selected + self.matches.len() - 1) % self.matches.len();
            }
            KeyCode::Backspace if plain => {
                self.query.pop();
                self.filter();
            }
            KeyCode::Char(c) if plain && !c.is_control() && self.query.chars().count() < 256 => {
                self.query.push(c);
                self.filter();
            }
            _ => {}
        }
        true
    }
    pub(crate) fn draw(
        &self,
        writer: &mut dyn Write,
        cols: u16,
        floor_row: u16,
        bg: Option<Color>,
    ) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        let count = self
            .matches
            .len()
            .min(10)
            .min(floor_row.saturating_sub(3) as usize);
        let top = floor_row.saturating_sub(count as u16 + 3);
        let color = |color| crate::ui::utils::resolve_color(color, self.monochrome);
        let line = |writer: &mut dyn Write, row: u16, text: &str, fg: Color| -> io::Result<()> {
            if row >= floor_row {
                return Ok(());
            }
            write!(writer, "{}", MoveTo(0, row))?;
            if let Some(bg) = bg {
                write!(writer, "{}", SetBackgroundColor(color(bg)))?;
            }
            write!(
                writer,
                "{}{}",
                Clear(ClearType::CurrentLine),
                SetForegroundColor(color(fg))
            )?;
            let safe = super::display_safe(text);
            write!(
                writer,
                "{}{}",
                crate::ui::utils::display_prefix(&safe, cols.saturating_sub(1) as usize),
                ResetColor
            )
        };
        line(
            writer,
            top,
            &format!(
                "[sessions] {}  [Tab] {}  [Enter] resume  [Esc] cancel",
                if self.all {
                    "All workspaces"
                } else {
                    "Current workspace"
                },
                if self.all {
                    "current workspace"
                } else {
                    "all workspaces"
                }
            ),
            Color::Cyan,
        )?;
        line(
            writer,
            top + 1,
            &format!("Filter: {}", self.query),
            Color::DarkGrey,
        )?;
        let start = self.selected.saturating_sub(count.saturating_sub(1));
        for (offset, index) in self.matches.iter().skip(start).take(count).enumerate() {
            let row = &self.catalogue.rows[*index];
            let selected = start + offset == self.selected;
            let text = format!(
                "{} {}  {}  {}  {} msgs  {}/{}",
                if selected { "▸" } else { " " },
                if row.title.is_empty() {
                    "(untitled)"
                } else {
                    &row.title
                },
                row.id.chars().take(8).collect::<String>(),
                relative_time(&row.updated_at),
                row.message_count,
                row.provider,
                row.model
            );
            let text = crate::ui::utils::middle_elide(&text, (cols as usize) * 2 / 3);
            let y = top + 2 + offset as u16;
            line(
                writer,
                y,
                &text,
                if selected { Color::Green } else { Color::White },
            )?;
            // Workspace labels use only persisted text; never resolve or open it.
            // Dim the current workspace while keeping foreign destinations visible.
            let label = format!(
                "  workspace:{}",
                if row.working_dir.is_empty() {
                    "(unbound)"
                } else {
                    &row.working_dir
                }
            );
            let safe = super::display_safe(&label);
            let room = (cols as usize).saturating_sub(crate::ui::utils::display_width(&text) + 1);
            write!(
                writer,
                "{} {}{}",
                SetForegroundColor(color(if row.current_workspace {
                    Color::DarkGrey
                } else {
                    Color::White
                })),
                crate::ui::utils::middle_elide(&safe, room),
                ResetColor
            )?;
        }
        if count == 0 {
            line(
                writer,
                top + 2,
                "No matching saved sessions",
                Color::DarkGrey,
            )?;
        }
        let note = format!(
            "{} shown{}{}",
            self.matches.len(),
            if self.catalogue.truncated {
                "; catalogue limit reached"
            } else {
                ""
            },
            if self.catalogue.skipped > 0 {
                "; unsafe/invalid/oversized entries skipped"
            } else {
                ""
            }
        );
        line(writer, floor_row.saturating_sub(1), &note, Color::DarkGrey)?;
        writer.flush()
    }
}

pub(crate) fn relative_time(timestamp: &str) -> String {
    let Ok(time) = chrono::DateTime::parse_from_rfc3339(timestamp) else {
        return "time unknown".into();
    };
    let seconds = (chrono::Utc::now() - time.with_timezone(&chrono::Utc))
        .num_seconds()
        .max(0);
    if seconds < 60 {
        "just now".into()
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86400)
    }
}

/// Startup selection happens before provider construction. Its native terminal
/// attachment is owned and restored on every return, including failed reloads.
pub(crate) fn choose(
    workspace: Arc<crate::paths::WorkspaceBinding>,
    all: bool,
    monochrome: bool,
) -> anyhow::Result<Option<SessionPick>> {
    let _terminal =
        crate::ui::terminal::TerminalGuard::new(crate::ui::terminal::TerminalOptions {
            mouse_capture: false,
            title_status: false,
        })?;
    let mut picker = SessionsPicker::new(
        crate::session::catalogue::list(&workspace, all)?,
        workspace.clone(),
        all,
    );
    picker.set_monochrome(monochrome);
    loop {
        let (cols, rows) = crossterm::terminal::size()?;
        picker.draw(&mut std::io::stdout(), cols, rows, None)?;
        match crossterm::event::read()? {
            crossterm::event::Event::Key(key)
                if key.kind == crossterm::event::KeyEventKind::Press =>
            {
                picker.handle(key);
            }
            crossterm::event::Event::Resize(..) => {}
            _ => continue,
        }
        match picker.take_outcome() {
            Some(SessionPick::Reload(all)) => {
                picker = SessionsPicker::new(
                    crate::session::catalogue::list(&workspace, all)?,
                    workspace.clone(),
                    all,
                );
                picker.set_monochrome(monochrome);
            }
            Some(SessionPick::Cancelled) => return Ok(None),
            Some(selected) => return Ok(Some(selected)),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn session_picker_keeps_unicode_query_row_identity_and_explicit_all_toggle() {
        let root = crate::tests::OwnedTestDirectory::new("session-picker");
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let row = crate::session::catalogue::SessionRow {
            id: uuid::Uuid::new_v4().to_string().into(),
            title: "Parser 雪".into(),
            updated_at: chrono::Utc::now().to_rfc3339().into(),
            working_dir: workspace.root().to_string_lossy().into(),
            message_count: 7,
            provider: "anthropic".into(),
            model: "test".into(),
            current_workspace: true,
        };
        let id = row.id.clone();
        let mut picker = SessionsPicker::new(
            Catalogue {
                rows: vec![row],
                ..Default::default()
            },
            workspace.clone(),
            false,
        );
        picker.paste("雪\u{1b}");
        assert_eq!(picker.query, "雪");
        assert_eq!(picker.matches, vec![0]);
        let mut bytes = Vec::new();
        picker.draw(&mut bytes, 160, 20, None).unwrap();
        let rendered = String::from_utf8(bytes).unwrap();
        assert!(rendered.contains("Current workspace"));
        assert!(rendered.contains("7 msgs"));
        assert!(rendered.contains("workspace:"));
        assert!(rendered.contains("雪"));
        picker.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match picker.take_outcome().unwrap() {
            SessionPick::Selected(selected) => {
                assert_eq!(selected.row.id, id);
                assert!(!selected.all);
                assert!(Arc::ptr_eq(&selected.workspace, &workspace));
            }
            _ => panic!("expected exact selection"),
        }
        picker.active = true;
        picker.handle(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(matches!(
            picker.take_outcome(),
            Some(SessionPick::Reload(true))
        ));
        picker.active = true;
        picker.handle(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(
            picker.take_outcome(),
            Some(SessionPick::Cancelled)
        ));
    }
}
