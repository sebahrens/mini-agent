use super::draw_picker_list;

/// Slash commands that are always available, regardless of which optional
/// features were compiled in. Feature-gated commands are appended by
/// [`available_commands`].
///
/// Kept in alphabetical order for ease of maintenance.
const BASE_COMMANDS: &[&str] = &[
    "/add",
    "/agent",
    "/btw",
    "/clear",
    "/compact",
    "/compress",
    "/drop",
    "/drop-all",
    "/editsys",
    "/exit",
    "/help",
    "/history",
    "/init",
    "/memory",
    "/mode",
    "/model",
    "/models",
    "/models-add",
    "/new",
    "/prompt",
    "/provider",
    "/queue",
    "/quit",
    "/reasoning",
    "/redo",
    "/regen-prompts",
    "/regen-themes",
    "/rename",
    "/retry",
    "/review",
    "/rewind",
    "/sessions",
    "/theme",
    "/thinking",
    "/toggle",
    "/tutor",
    "/tutorial",
    "/undo",
    "/welcome",
];

/// Build the autocomplete command list, including only the commands whose
/// backing feature was actually compiled in.
///
/// `#[cfg]` cannot be attached to elements of an array literal on stable Rust
/// (that requires the unstable `stmt_expr_attributes` feature), so the
/// feature-gated commands are appended via conditionally-compiled statements
/// instead. Feature blocks are ordered alphabetically by feature name, and the
/// commands within each block are likewise alphabetical. The result is sorted,
/// so ranking ties fall back to name order. Every command `/help` lists must
/// appear here; a test in `crate::ui::slash::help` enforces that.
pub(crate) fn available_commands() -> Vec<&'static str> {
    let mut cmds: Vec<&'static str> = BASE_COMMANDS.to_vec();

    #[cfg(feature = "advisor")]
    cmds.push("/advisor");

    #[cfg(feature = "export")]
    {
        cmds.push("/export");
        cmds.push("/import");
        cmds.push("/share");
    }

    #[cfg(feature = "goal")]
    cmds.push("/goal");

    #[cfg(feature = "git-worktree")]
    {
        cmds.push("/worktree");
        cmds.push("/wt-exit");
        cmds.push("/wt-merge");
    }

    #[cfg(feature = "hooks")]
    cmds.push("/hooks");

    #[cfg(feature = "loop")]
    cmds.push("/loop");

    #[cfg(feature = "mcp")]
    cmds.push("/mcp");

    #[cfg(feature = "subagents")]
    {
        cmds.push("/model-subagent");
        cmds.push("/models-subagent");
    }

    cmds.sort_unstable();
    cmds
}

/// Rank how `name` matches a lowercase `query`, lower is better, or `None` when
/// it does not contain the query. A leading `/` is ignored so `/re` and `re`
/// rank the same: exact name, then prefix, then a match starting at a word
/// boundary, then any other substring.
pub(crate) fn match_rank(name: &str, query_lower: &str) -> Option<u8> {
    let name_lower = name.to_lowercase();
    let bare = name_lower.strip_prefix('/').unwrap_or(&name_lower);
    let query = query_lower.strip_prefix('/').unwrap_or(query_lower);
    if query.is_empty() {
        return Some(0);
    }
    if bare == query {
        return Some(0);
    }
    if bare.starts_with(query) {
        return Some(1);
    }
    let position = bare.find(query)?;
    let at_boundary = bare[..position]
        .chars()
        .next_back()
        .is_some_and(|previous| matches!(previous, '-' | '_' | '.' | ' ' | ':' | '/'));
    Some(if at_boundary { 2 } else { 3 })
}

pub struct ListPicker {
    pub active: bool,
    pub query: String,
    pub cursor: usize,
    pub matches: Vec<String>,
    pub selected: usize,
    items: Vec<String>,
    /// One-line description per entry of `items`, shown after the name;
    /// empty when the list has none. Filtering only looks at the names.
    descriptions: Vec<String>,
    /// The entry marked `(current)` and highlighted while the query is empty.
    current: Option<String>,
    monochrome: bool,
}

impl ListPicker {
    pub fn new() -> Self {
        ListPicker {
            active: false,
            query: String::new(),
            cursor: 0,
            matches: Vec::new(),
            selected: 0,
            items: Vec::new(),
            descriptions: Vec::new(),
            current: None,
            monochrome: false,
        }
    }

    pub fn with_static_commands() -> Self {
        let mut picker = ListPicker::new();
        picker.items = available_commands().iter().map(|s| s.to_string()).collect();
        picker
    }

    pub fn set_monochrome(&mut self, monochrome: bool) {
        self.monochrome = monochrome;
    }

    pub fn set_items(&mut self, items: Vec<String>) {
        self.items = items;
        self.descriptions.clear();
    }

    /// Set `(name, description)` entries. Names are matched and inserted;
    /// descriptions are only drawn.
    pub fn set_described_items(&mut self, entries: Vec<(String, String)>) {
        (self.items, self.descriptions) = entries.into_iter().unzip();
    }

    /// Mark `name` as the current value: it is labelled `(current)` and
    /// highlighted whenever the query is empty.
    pub fn set_current(&mut self, name: Option<String>) {
        self.current = name;
    }

    /// The rows the overlay draws for `matches`: the bare name, or for
    /// described lists the padded name, its description and a `(current)`
    /// marker on the current entry.
    pub(crate) fn display_rows(&self) -> Vec<String> {
        if self.descriptions.is_empty() {
            return self.matches.clone();
        }
        let width = self
            .items
            .iter()
            .map(|n| n.chars().count())
            .max()
            .unwrap_or(0);
        self.matches
            .iter()
            .map(|name| {
                let description = self
                    .items
                    .iter()
                    .position(|item| item == name)
                    .and_then(|index| self.descriptions.get(index))
                    .map(String::as_str)
                    .unwrap_or("");
                let marker = if self.current.as_deref() == Some(name.as_str()) {
                    "  (current)"
                } else {
                    ""
                };
                format!("{name:<width$}  {description}{marker}")
            })
            .collect()
    }

    pub fn activate(&mut self) {
        self.active = true;
        self.query.clear();
        self.cursor = 0;
        self.matches.clear();
        self.selected = 0;
        self.filter();
    }

    pub fn deactivate(&mut self) {
        self.active = false;
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
        let query_lower = self.query.to_lowercase();
        let mut ranked: Vec<(u8, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, name)| match_rank(name, &query_lower).map(|rank| (rank, index)))
            .collect();
        // Stable on the caller's order: for commands that order is alphabetical.
        ranked.sort_unstable();
        self.matches = ranked
            .into_iter()
            .take(50)
            .map(|(_, index)| self.items[index].clone())
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

    pub fn draw(&self, empty_message: Option<&str>, floor_row: u16) -> std::io::Result<()> {
        if !self.active {
            return Ok(());
        }
        draw_picker_list(
            &self.display_rows(),
            self.selected,
            self.monochrome,
            empty_message,
            floor_row,
            0,
        )
    }
}
