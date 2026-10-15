use std::borrow::Cow;
use std::sync::OnceLock;

use super::draw_picker_rows;

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
    "/compress",
    "/config",
    "/context",
    "/cost",
    "/doctor",
    "/drop",
    "/drop-all",
    "/editsys",
    "/effort",
    "/fork",
    "/help",
    "/history",
    "/init",
    "/jobs",
    "/keys",
    "/login",
    "/logout",
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
    "/sandbox",
    "/sessions",
    "/steer",
    "/theme",
    "/toggle",
    "/tutor",
    "/undo",
    "/vim",
    "/welcome",
];

/// Compatibility aliases, `(alias, canonical)`. They still dispatch, and `/help`
/// mentions them next to their command, but completion offers only the
/// canonical name so each command appears once.
pub(crate) const COMMAND_ALIASES: &[(&str, &str)] = &[
    ("/compact", "/compress"),
    ("/exit", "/quit"),
    ("/model-subagent", "/subagent-model"),
    ("/models-subagent", "/subagent-models"),
    ("/thinking", "/reasoning"),
    ("/tutorial", "/welcome"),
];

/// Whether `name` is a hidden compatibility alias.
pub(crate) fn is_command_alias(name: &str) -> bool {
    COMMAND_ALIASES.iter().any(|(alias, _)| *alias == name)
}

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
    static_commands().0.clone()
}

/// The sorted command list and its lowercase forms, built once per process:
/// the slash picker opens on every `/` and borrows these instead of
/// rebuilding and re-sorting the list.
fn static_commands() -> &'static (Vec<&'static str>, Vec<String>) {
    static COMMANDS: OnceLock<(Vec<&'static str>, Vec<String>)> = OnceLock::new();
    COMMANDS.get_or_init(|| {
        let commands = build_available_commands();
        let lower = commands.iter().map(|name| name.to_lowercase()).collect();
        (commands, lower)
    })
}

fn build_available_commands() -> Vec<&'static str> {
    let mut cmds: Vec<&'static str> = BASE_COMMANDS.to_vec();

    #[cfg(feature = "agent-skills")]
    cmds.push("/skills");
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
        cmds.push("/commit");
        cmds.push("/diff");
        cmds.push("/pr");
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
        cmds.push("/subagent-model");
        cmds.push("/subagent-models");
        cmds.push("/tasks");
    }

    cmds.sort_unstable();
    cmds
}

/// Reserve builtins even when their optional feature is disabled, so a
/// template never silently changes meaning after a feature/build switch.
pub(crate) fn reserved_command(name: &str) -> bool {
    let slash = format!("/{name}");
    BASE_COMMANDS.contains(&slash.as_str())
        || COMMAND_ALIASES.iter().any(|(alias, _)| *alias == slash)
        || [
            "/skills",
            "/advisor",
            "/export",
            "/import",
            "/share",
            "/goal",
            "/worktree",
            "/wt-exit",
            "/wt-merge",
            "/hooks",
            "/loop",
            "/mcp",
            "/subagent-model",
            "/subagent-models",
            "/tasks",
            "/diff",
            "/commit",
            "/pr",
        ]
        .contains(&slash.as_str())
}

/// Rank how `name` matches a lowercase `query`, lower is better, or `None` when
/// it does not contain the query. A leading `/` is ignored so `/re` and `re`
/// rank the same: exact name, then prefix, then a match starting at a word
/// boundary, then any other substring.
pub(crate) fn match_rank(name: &str, query_lower: &str) -> Option<u8> {
    match_rank_lower(&name.to_lowercase(), query_lower)
}

/// [`match_rank`] for an already-lowercased name.
fn match_rank_lower(name_lower: &str, query_lower: &str) -> Option<u8> {
    let bare = name_lower.strip_prefix('/').unwrap_or(name_lower);
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
    /// Index into `items` of each entry of `matches`.
    match_indices: Vec<usize>,
    pub selected: usize,
    items: Vec<Cow<'static, str>>,
    /// Lowercase form of each entry of `items`, computed when items are set.
    items_lower: Vec<Cow<'static, str>>,
    /// Widest item name in characters (the description column), computed
    /// when items are set.
    name_width: usize,
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
            match_indices: Vec::new(),
            selected: 0,
            items: Vec::new(),
            items_lower: Vec::new(),
            name_width: 0,
            descriptions: Vec::new(),
            current: None,
            monochrome: false,
        }
    }

    pub fn with_static_commands() -> Self {
        let (commands, lower) = static_commands();
        let mut picker = ListPicker::new();
        picker.items = commands.iter().map(|name| Cow::Borrowed(*name)).collect();
        picker.items_lower = lower
            .iter()
            .map(|name| Cow::Borrowed(name.as_str()))
            .collect();
        picker.name_width = width_of(&picker.items);
        picker
    }

    pub(crate) fn append_described_items(&mut self, entries: Vec<(String, String)>) {
        self.descriptions.resize(self.items.len(), String::new());
        for (name, description) in entries {
            if !self.items.iter().any(|item| *item == name) {
                self.items_lower.push(Cow::Owned(name.to_lowercase()));
                self.name_width = self.name_width.max(name.chars().count());
                self.items.push(Cow::Owned(name));
                self.descriptions.push(description);
            }
        }
    }

    fn set_names(&mut self, items: Vec<String>) {
        self.items_lower = items
            .iter()
            .map(|name| Cow::Owned(name.to_lowercase()))
            .collect();
        self.items = items.into_iter().map(Cow::Owned).collect();
        self.name_width = width_of(&self.items);
    }

    pub fn set_monochrome(&mut self, monochrome: bool) {
        self.monochrome = monochrome;
    }

    #[cfg(test)]
    pub(crate) fn is_monochrome(&self) -> bool {
        self.monochrome
    }

    pub fn set_items(&mut self, items: Vec<String>) {
        self.set_names(items);
        self.descriptions.clear();
    }

    /// Set `(name, description)` entries. Names are matched and inserted;
    /// descriptions are only drawn.
    pub fn set_described_items(&mut self, entries: Vec<(String, String)>) {
        let (items, descriptions) = entries.into_iter().unzip();
        self.set_names(items);
        self.descriptions = descriptions;
    }

    /// Mark `name` as the current value: it is labelled `(current)` and
    /// highlighted whenever the query is empty.
    pub fn set_current(&mut self, name: Option<String>) {
        self.current = name;
    }

    /// The rows the overlay draws for `matches`: the bare name, or for
    /// described lists the padded name, its description and a `(current)`
    /// marker on the current entry.
    #[cfg(test)]
    pub(crate) fn display_rows(&self) -> Vec<String> {
        (0..self.matches.len())
            .map(|index| self.display_row(index).into_owned())
            .collect()
    }

    /// The drawn row for `matches[index]`; only visible rows are formatted.
    pub(crate) fn display_row(&self, index: usize) -> Cow<'_, str> {
        let name = self.matches[index].as_str();
        if self.descriptions.is_empty() {
            return Cow::Borrowed(name);
        }
        let description = self
            .match_indices
            .get(index)
            .and_then(|item| self.descriptions.get(*item))
            .map(String::as_str)
            .unwrap_or("");
        let marker = if self.current.as_deref() == Some(name) {
            "  (current)"
        } else {
            ""
        };
        let width = self.name_width;
        Cow::Owned(format!("{name:<width$}  {description}{marker}"))
    }

    pub fn activate(&mut self) {
        self.active = true;
        self.query.clear();
        self.cursor = 0;
        self.matches.clear();
        self.match_indices.clear();
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
            .items_lower
            .iter()
            .enumerate()
            .filter_map(|(index, name)| {
                match_rank_lower(name, &query_lower).map(|rank| (rank, index))
            })
            .collect();
        // Stable on the caller's order: for commands that order is alphabetical.
        ranked.sort_unstable();
        self.match_indices = ranked
            .into_iter()
            .take(50)
            .map(|(_, index)| index)
            .collect();
        self.matches = self
            .match_indices
            .iter()
            .map(|index| self.items[*index].to_string())
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

    pub fn draw(
        &self,
        out: &mut dyn std::io::Write,
        cols: u16,
        empty_message: Option<&str>,
        floor_row: u16,
    ) -> std::io::Result<()> {
        if !self.active {
            return Ok(());
        }
        draw_picker_rows(
            out,
            cols,
            self.matches.len(),
            |index| self.display_row(index),
            self.selected,
            self.monochrome,
            empty_message,
            floor_row,
            0,
        )
    }
}

fn width_of(items: &[Cow<'static, str>]) -> usize {
    items
        .iter()
        .map(|name| name.chars().count())
        .max()
        .unwrap_or(0)
}
