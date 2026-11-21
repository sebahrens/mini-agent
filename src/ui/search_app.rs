//! Transcript search: find text in the feed, jump between matches.
//!
//! The TUI draws on the alternate screen with its own scrollback, so the
//! terminal's native find cannot see earlier output. `ctrl+f`-style search
//! (`transcript_search`, default `alt+s`) and `/find <text>` open a one-line
//! query in the prompt area; matches are rows of the laid-out transcript
//! (smart case), a collapsed tool result that holds the text only in its
//! hidden form is unfolded when stepped to, and the match is scrolled to the
//! middle of the viewport and highlighted. Esc restores the scroll position
//! the search started from; `q` leaves the view where it is.
use super::*;
use crate::ui::feed::SearchHit;
use crate::ui::renderer::PermissionPrompt;

pub(super) struct TranscriptSearch {
    query: String,
    /// Typing the query (Enter stops typing) rather than stepping matches.
    typing: bool,
    /// Scroll offset when the search opened, restored by Esc.
    saved_offset: usize,
    hits: Vec<SearchHit>,
    /// Index into `hits` of the match shown.
    current: Option<usize>,
}

impl App<'_> {
    pub(super) fn start_transcript_search(&mut self, query: Option<&str>) -> anyhow::Result<()> {
        self.transcript_search = Some(TranscriptSearch {
            query: String::new(),
            typing: query.is_none(),
            saved_offset: self.renderer.scroll_offset(),
            hits: Vec::new(),
            current: None,
        });
        if let Some(query) = query {
            if let Some(search) = self.transcript_search.as_mut() {
                search.query = query.to_string();
            }
            self.update_search(true)?;
            // `/find text` steps straight to the first match.
        } else {
            self.show_search_prompt()?;
        }
        Ok(())
    }

    pub(super) fn close_transcript_search(&mut self, restore: bool) -> anyhow::Result<()> {
        let Some(search) = self.transcript_search.take() else {
            return Ok(());
        };
        if self.permission_prompt.is_none() && self.question_prompt.is_none() {
            self.renderer.permission_prompt = None;
        }
        if restore {
            self.renderer.restore_scroll_offset(search.saved_offset);
        } else {
            self.renderer.clear_selection();
        }
        self.refresh()?;
        Ok(())
    }

    fn show_search_prompt(&mut self) -> anyhow::Result<()> {
        if self.permission_prompt.is_some() || self.question_prompt.is_some() {
            return Ok(());
        }
        let Some(search) = self.transcript_search.as_ref() else {
            return Ok(());
        };
        let position = match (search.current, search.hits.len()) {
            (_, 0) if search.query.is_empty() => "type to search".to_string(),
            (_, 0) => "no match".to_string(),
            (Some(current), total) => format!("{}/{}", current + 1, total),
            (None, total) => format!("{total} matches"),
        };
        let keys = if search.typing {
            "[Enter] step through matches  [Esc] close and go back"
        } else {
            "[n/Enter] next  [N/Shift+Enter] previous  [/] edit  [q] stay here  [Esc] go back"
        };
        let cursor = if search.typing { "▏" } else { "" };
        self.renderer.permission_prompt = Some(PermissionPrompt {
            tool: format!("find in transcript ({position})").into(),
            options: format!(
                "find: {}{cursor}\n{keys}",
                crate::ui::events::sanitize_for_review(&search.query)
            )
            .into(),
            expanded: true,
        });
        crate::ui::permission_handler::draw_prompt(&mut self.renderer)?;
        Ok(())
    }

    /// Recompute the matches for the current query. With `from_top` the first
    /// match at or after the current scroll position becomes the current one.
    fn update_search(&mut self, jump: bool) -> anyhow::Result<()> {
        let Some(query) = self.transcript_search.as_ref().map(|s| s.query.clone()) else {
            return Ok(());
        };
        let hits = self.renderer.search_transcript(&query);
        let first_visible_line = {
            let total = self.renderer.buffer_len();
            let visible = self.renderer.visible_lines();
            total.saturating_sub(self.renderer.scroll_offset() + visible)
        };
        let current = if hits.is_empty() {
            None
        } else {
            // Start from the matches at or below the viewport top, so a search
            // begun while scrolled does not jump to the very beginning.
            Some(
                hits.iter()
                    .position(|hit| hit.line >= first_visible_line)
                    .unwrap_or(0),
            )
        };
        if let Some(search) = self.transcript_search.as_mut() {
            search.hits = hits;
            search.current = current;
        }
        if jump {
            self.show_current_hit()?;
        }
        self.show_search_prompt()
    }

    /// Scroll to and highlight the current hit, unfolding it first when it is
    /// hidden in a collapsed block.
    fn show_current_hit(&mut self) -> anyhow::Result<()> {
        let Some(search) = self.transcript_search.as_ref() else {
            return Ok(());
        };
        let Some(hit) = search
            .current
            .and_then(|index| search.hits.get(index))
            .cloned()
        else {
            return Ok(());
        };
        if hit.hidden {
            self.renderer.reveal_block(hit.block);
            let query = search.query.clone();
            let hits = self.renderer.search_transcript(&query);
            // The unfolded block now has row-level hits; continue from its first.
            let current = hits.iter().position(|h| h.block == hit.block && !h.hidden);
            if let Some(search) = self.transcript_search.as_mut() {
                search.hits = hits;
                search.current = current
                    .or(search.current)
                    .filter(|i| *i < search.hits.len());
            }
        }
        let Some(search) = self.transcript_search.as_ref() else {
            return Ok(());
        };
        if let Some(hit) = search
            .current
            .and_then(|index| search.hits.get(index))
            .cloned()
        {
            self.renderer.show_search_hit(&hit)?;
        }
        Ok(())
    }

    fn step_search(&mut self, forward: bool) -> anyhow::Result<()> {
        if let Some(search) = self.transcript_search.as_mut() {
            let total = search.hits.len();
            if total > 0 {
                search.current = Some(match search.current {
                    Some(current) if forward => (current + 1) % total,
                    Some(current) => (current + total - 1) % total,
                    None => 0,
                });
            }
        }
        self.show_current_hit()?;
        self.show_search_prompt()
    }

    /// Handle one key while the search is open. Always consumes the key.
    pub(super) fn handle_search_key(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        use crossterm::event::KeyEventKind;
        if key.kind == KeyEventKind::Release {
            return Ok(());
        }
        let Some(typing) = self.transcript_search.as_ref().map(|s| s.typing) else {
            return Ok(());
        };
        let plain = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
        match key.code {
            KeyCode::Esc => return self.close_transcript_search(true),
            KeyCode::Char('c' | 'd') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return self.close_transcript_search(true);
            }
            KeyCode::Enter if typing => {
                if let Some(search) = self.transcript_search.as_mut() {
                    if search.query.is_empty() {
                        return self.close_transcript_search(true);
                    }
                    search.typing = false;
                }
                self.step_search(true)?;
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.step_search(false)?
            }
            KeyCode::Enter => self.step_search(true)?,
            KeyCode::Backspace if typing => {
                if let Some(search) = self.transcript_search.as_mut() {
                    search.query.pop();
                }
                self.update_search(true)?;
            }
            KeyCode::Char(c) if typing && plain && !c.is_control() => {
                if let Some(search) = self.transcript_search.as_mut()
                    && search.query.len() + c.len_utf8() <= 256
                {
                    search.query.push(c);
                }
                self.update_search(true)?;
            }
            KeyCode::Char('n') if !typing && plain => self.step_search(true)?,
            KeyCode::Char('N') if !typing && plain => self.step_search(false)?,
            KeyCode::Char('/') if !typing && plain => {
                if let Some(search) = self.transcript_search.as_mut() {
                    search.typing = true;
                }
                self.show_search_prompt()?;
            }
            KeyCode::Char('q') if !typing && plain => return self.close_transcript_search(false),
            KeyCode::PageUp | KeyCode::PageDown | KeyCode::Up | KeyCode::Down => {
                // Manual scrolling leaves the match highlight where it was.
                match key.code {
                    KeyCode::PageUp => self.renderer.scroll_page_up(),
                    KeyCode::PageDown => self.renderer.scroll_page_down(),
                    KeyCode::Up => self.renderer.scroll_line_up(),
                    _ => self.renderer.scroll_line_down(),
                }
                self.renderer.render_viewport()?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Open the search from `/find <text>`; an empty query opens the prompt.
    pub(super) fn run_find_command(&mut self, text: &str) -> anyhow::Result<()> {
        let query = text.trim_start_matches("/find").trim();
        if query.is_empty() {
            self.start_transcript_search(None)
        } else {
            self.start_transcript_search(Some(query))
        }
    }
}

pub(super) fn is_find_command(text: &str) -> bool {
    text == "/find" || text.starts_with("/find ")
}
