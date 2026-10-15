use std::borrow::Cow;
use std::io::Write;

use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::{Color, ResetColor, SetForegroundColor};
use crossterm::terminal::Clear;

use super::super::utils::resolve_color;
use super::{draw_picker_rows, fuzzy_score_lower};

pub struct ModelsPicker {
    pub active: bool,
    pub query: String,
    pub cursor: usize,
    pub matches: Vec<String>,
    pub selected: usize,
    quick: Vec<String>,
    provider: Vec<String>,
    /// Lowercase forms of `quick` and `provider`, computed in `set_groups`.
    quick_lower: Vec<String>,
    provider_lower: Vec<String>,
    pub group: usize,
    monochrome: bool,
    /// The command whose argument this picker completes: `/model ` (the
    /// selector) or `/models ` (kept for compatibility).
    pub prefix: &'static str,
    /// The session's current model, marked `(current)` and highlighted
    /// while the query is empty.
    current: Option<String>,
}

impl ModelsPicker {
    pub fn new() -> Self {
        ModelsPicker {
            active: false,
            query: String::new(),
            cursor: 0,
            matches: Vec::new(),
            selected: 0,
            quick: Vec::new(),
            provider: Vec::new(),
            quick_lower: Vec::new(),
            provider_lower: Vec::new(),
            group: 0,
            monochrome: false,
            prefix: "/models ",
            current: None,
        }
    }

    /// A picker completing `/model <name>` with the current model marked.
    pub fn for_command(prefix: &'static str, current: Option<String>) -> Self {
        ModelsPicker {
            prefix,
            current,
            ..ModelsPicker::new()
        }
    }

    /// The rows drawn for `matches`, with `(current)` after the current model.
    #[cfg(test)]
    pub(crate) fn display_rows(&self) -> Vec<String> {
        (0..self.matches.len())
            .map(|index| self.display_row(index).into_owned())
            .collect()
    }

    /// The drawn row for `matches[index]`; only visible rows are formatted.
    fn display_row(&self, index: usize) -> Cow<'_, str> {
        let name = self.matches[index].as_str();
        if self.current.as_deref() == Some(name) {
            Cow::Owned(format!("{name}  (current)"))
        } else {
            Cow::Borrowed(name)
        }
    }

    pub fn set_monochrome(&mut self, monochrome: bool) {
        self.monochrome = monochrome;
    }

    #[cfg(test)]
    pub(crate) fn is_monochrome(&self) -> bool {
        self.monochrome
    }

    pub fn set_groups(&mut self, quick: Vec<String>, provider: Vec<String>) {
        self.quick_lower = quick.iter().map(|name| name.to_lowercase()).collect();
        self.provider_lower = provider.iter().map(|name| name.to_lowercase()).collect();
        self.quick = quick;
        self.provider = provider;
    }

    fn color(&self, color: Color) -> Color {
        resolve_color(color, self.monochrome)
    }

    pub fn activate(&mut self) {
        self.active = true;
        self.query.clear();
        self.cursor = 0;
        self.matches.clear();
        self.selected = 0;
        self.group = if self.quick.is_empty() && !self.provider.is_empty() {
            1
        } else {
            0
        };
        self.filter();
    }

    pub fn deactivate(&mut self) {
        self.active = false;
    }

    pub fn toggle_group(&mut self) {
        self.group = 1 - self.group;
        self.selected = 0;
        self.filter();
    }

    pub fn char_input(&mut self, c: char) {
        let byte_pos = self
            .query
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.query.len());
        self.query.insert(byte_pos, c);
        self.cursor += 1;
        self.filter();
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 && !self.query.is_empty() {
            self.cursor -= 1;
            let byte_pos = self
                .query
                .char_indices()
                .nth(self.cursor)
                .map(|(i, _)| i)
                .unwrap_or(self.query.len());
            self.query.remove(byte_pos);
            self.filter();
        }
    }

    fn filter(&mut self) {
        let (src, lower) = if self.group == 0 {
            (&self.quick, &self.quick_lower)
        } else {
            (&self.provider, &self.provider_lower)
        };
        let query_lower = self.query.to_lowercase();
        let mut scored: Vec<(i32, &String)> = src
            .iter()
            .zip(lower)
            .filter_map(|(n, l)| fuzzy_score_lower(l, &query_lower).map(|s| (s, n)))
            .collect();
        scored.sort_by_key(|b| std::cmp::Reverse(b.0));
        self.matches = scored
            .into_iter()
            .take(50)
            .map(|(_, n)| n.clone())
            .collect();
        self.selected = if self.query.is_empty() {
            self.current
                .as_ref()
                .and_then(|current| self.matches.iter().position(|m| m == current))
                .unwrap_or(0)
        } else {
            0
        };
    }

    pub fn select_next(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1) % self.matches.len();
        }
    }

    pub fn select_prev(&mut self) {
        if !self.matches.is_empty() {
            self.selected = if self.selected == 0 {
                self.matches.len() - 1
            } else {
                self.selected - 1
            };
        }
    }

    pub fn selected_name(&self) -> Option<&str> {
        self.matches.get(self.selected).map(|s| s.as_str())
    }

    pub fn draw(&self, mut out: &mut dyn Write, cols: u16, floor_row: u16) -> std::io::Result<()> {
        if !self.active {
            return Ok(());
        }
        let window = super::picker_window(floor_row, 1, self.matches.len().max(1), self.selected);

        if let Some(header_row) = window.top_row.checked_sub(1)
            && floor_row >= 2
        {
            queue!(
                &mut out,
                super::picker_command(MoveTo(0, header_row)),
                super::picker_command(Clear(crossterm::terminal::ClearType::CurrentLine)),
            )?;
            let tab = |label: &str, count: usize, active: bool| {
                if active {
                    format!("[{} {}]", label, count)
                } else {
                    format!(" {} {} ", label, count)
                }
            };
            write!(
                out,
                "{}{}  {}   (Tab to switch groups · /models refresh for the latest){}",
                SetForegroundColor(self.color(Color::DarkGrey)),
                tab("Quick", self.quick.len(), self.group == 0),
                tab("Provider", self.provider.len(), self.group == 1),
                ResetColor
            )?;
        }

        draw_picker_rows(
            out,
            cols,
            self.matches.len(),
            |index| self.display_row(index),
            self.selected,
            self.monochrome,
            None,
            floor_row,
            1,
        )
    }
}
