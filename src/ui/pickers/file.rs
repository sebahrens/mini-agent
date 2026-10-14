use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use crossterm::ExecutableCommand;
use crossterm::cursor::MoveTo;
use crossterm::style::{Color, ResetColor, SetForegroundColor};
use crossterm::terminal::Clear;

use super::super::utils::resolve_color;

/// Paths per batch sent from the background walk to the picker. Small
/// batches keep the first matches visible quickly on large trees.
const WALK_BATCH_SIZE: usize = 25;

/// Cap on walked files. Directories are listed too but do not count, so a
/// wide tree of folders cannot starve the files that sort after them.
pub(crate) const MAX_WALK_FILES: usize = 20_000;

/// Safety cap on all emitted entries (files plus directories) so a tree made
/// almost entirely of empty directories still terminates promptly.
const MAX_WALK_ENTRIES: usize = 50_000;

/// Deepest directory level the walk descends into.
const MAX_WALK_DEPTH: usize = 16;

#[cfg(all(test, feature = "mcp"))]
mod mcp_resource_tests {
    use super::*;
    #[test]
    fn opaque_resource_completion_preserves_the_operand_without_opening_a_walk() {
        let operand = "mcp:owned:urn%3A%E9%9B%AA";
        let mut picker = FilePicker::new();
        picker.append_virtual_items(std::iter::once(PathBuf::from(format!("@{operand}"))));
        for character in "mcp:owned:".chars() {
            picker.char_input(character);
        }
        assert_eq!(picker.matches, vec![PathBuf::from(format!("@{operand}"))]);
        assert!(!picker.is_loading());
        let mut buffer: compact_str::CompactString = "inspect @mcp:owned:".into();
        let mut cursor = buffer.len();
        assert!(super::super::handlers::handle_file_key(
            &mut buffer,
            &mut cursor,
            &mut picker,
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Tab,
                crossterm::event::KeyModifiers::NONE
            ),
        ));
        assert_eq!(buffer, format!("inspect @{operand}"));
        assert_eq!(cursor, buffer.len());
    }
}

pub struct FilePicker {
    pub active: bool,
    pub query: String,
    pub cursor: usize,
    pub matches: Vec<PathBuf>,
    pub selected: usize,
    pub(crate) argument_start: Option<usize>,
    file_cache: Vec<PathBuf>,
    monochrome: bool,
    loading: bool,
    walk_rx: Option<mpsc::Receiver<Vec<PathBuf>>>,
    walk_cancel: Arc<AtomicBool>,
    walk_workspace: Option<Arc<crate::paths::WorkspaceBinding>>,
}

impl FilePicker {
    pub fn new() -> Self {
        FilePicker {
            active: false,
            query: String::new(),
            cursor: 0,
            matches: Vec::new(),
            selected: 0,
            argument_start: None,
            file_cache: Vec::new(),
            monochrome: false,
            loading: false,
            walk_rx: None,
            walk_cancel: Arc::new(AtomicBool::new(false)),
            walk_workspace: None,
        }
    }

    pub(crate) fn set_add_argument(&mut self, start: usize, query: &str) {
        self.argument_start = Some(start);
        self.query = query.to_owned();
        self.cursor = self.query.chars().count();
        self.filter();
    }

    pub fn set_monochrome(&mut self, monochrome: bool) {
        self.monochrome = monochrome;
    }

    /// Opaque MCP operands are completion strings, never filesystem walk roots.
    #[cfg(feature = "mcp")]
    pub(crate) fn append_virtual_items(&mut self, items: impl Iterator<Item = PathBuf>) {
        self.file_cache.extend(items);
        self.filter();
    }

    fn color(&self, color: Color) -> Color {
        resolve_color(color, self.monochrome)
    }

    pub fn activate(&mut self) {
        self.activate_at(None, false);
    }

    pub(crate) fn activate_for_add(
        &mut self,
        workspace: Option<Arc<crate::paths::WorkspaceBinding>>,
    ) {
        self.activate_at(workspace, true);
    }

    fn activate_at(
        &mut self,
        workspace: Option<Arc<crate::paths::WorkspaceBinding>>,
        require_binding: bool,
    ) {
        // Cancel any walk still running from a previous activation before
        // arming a fresh flag for the new one.
        self.walk_cancel.store(true, Ordering::Relaxed);
        self.walk_rx = None;

        self.active = true;
        self.argument_start = None;
        self.query.clear();
        self.cursor = 0;
        self.matches.clear();
        self.selected = 0;
        self.file_cache.clear();
        self.loading = false;
        self.walk_workspace = workspace.clone();
        if require_binding
            && workspace
                .as_ref()
                .is_none_or(|root| root.validate().is_err())
        {
            return;
        }

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            self.loading = true;
            self.walk_cancel = Arc::new(AtomicBool::new(false));
            let cancel = self.walk_cancel.clone();
            let (tx, rx) = mpsc::channel();
            self.walk_rx = Some(rx);
            handle.spawn_blocking(move || {
                if let Some(workspace) = workspace {
                    if workspace.validate().is_ok() {
                        walk_files_streaming_path(workspace.root(), &cancel, |batch| {
                            workspace.validate().is_ok() && tx.send(batch).is_ok()
                        });
                    }
                } else {
                    walk_files_streaming(".", &cancel, |batch| tx.send(batch).is_ok());
                }
            });
        } else {
            self.load_files_sync();
        }
    }

    fn load_files_sync(&mut self) {
        if let Some(workspace) = &self.walk_workspace {
            let mut files = Vec::new();
            walk_files_streaming_path(workspace.root(), &AtomicBool::new(false), |batch| {
                if workspace.validate().is_err() {
                    return false;
                }
                files.extend(batch);
                true
            });
            if workspace.validate().is_ok() {
                self.file_cache = files;
            }
        } else {
            self.file_cache = walk_files(".");
        }
        self.filter();
    }

    pub fn deactivate(&mut self) {
        self.active = false;
        // Stop the background walk (if any): the flag makes it exit early,
        // and dropping the receiver makes its next batch send fail.
        self.walk_cancel.store(true, Ordering::Relaxed);
        self.walk_rx = None;
        self.loading = false;
    }

    /// Whether the background walk is still streaming paths.
    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Drain walk batches that arrived since the last call. Returns true
    /// when new files arrived or the walk just finished.
    pub fn try_finish_loading(&mut self) -> bool {
        if self
            .walk_workspace
            .as_ref()
            .is_some_and(|root| root.validate().is_err())
        {
            let changed = self.active
                || self.loading
                || !self.file_cache.is_empty()
                || !self.matches.is_empty();
            self.file_cache.clear();
            self.matches.clear();
            self.deactivate();
            return changed;
        }
        if !self.loading {
            return false;
        }
        let mut changed = false;
        let mut done = false;
        if let Some(rx) = &self.walk_rx {
            loop {
                match rx.try_recv() {
                    Ok(batch) => {
                        self.file_cache.extend(batch);
                        changed = true;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        done = true;
                        break;
                    }
                }
            }
        }
        if done {
            self.loading = false;
            self.walk_rx = None;
        }
        if changed || done {
            self.filter();
        }
        changed || done
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
        // Filter even while the walk is streaming so matches update live.
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
        if self.file_cache.is_empty() {
            self.matches.clear();
            return;
        }
        let query_lower = self.query.to_lowercase();
        self.matches = self
            .file_cache
            .iter()
            .filter(|p| {
                let lower = p.to_string_lossy().to_lowercase();
                lower.contains(&query_lower)
            })
            .take(50)
            .cloned()
            .collect();
        self.selected = 0;
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

    pub fn selected_path(&self) -> Option<&PathBuf> {
        if self
            .walk_workspace
            .as_ref()
            .is_some_and(|root| root.validate().is_err())
        {
            return None;
        }
        self.matches.get(self.selected)
    }

    #[cfg(test)]
    pub fn test_set_cache(&mut self, files: Vec<PathBuf>) {
        self.file_cache = files;
        self.loading = false;
    }

    pub fn draw(&mut self, floor_row: u16) -> std::io::Result<()> {
        if !self.active {
            return Ok(());
        }

        self.try_finish_loading();
        if !self.active {
            return Ok(());
        }

        if self.loading && self.matches.is_empty() {
            return super::draw_picker_message("scanning files...", self.monochrome, floor_row);
        }
        if self.matches.is_empty() {
            return super::draw_picker_message("no matches", self.monochrome, floor_row);
        }

        let (cols, _rows) = crossterm::terminal::size()?;
        let mut stdout = std::io::stdout();
        let window = super::picker_window(floor_row, 0, self.matches.len(), self.selected);

        for i in window.start..window.end {
            let render_row = window.top_row + (i - window.start) as u16;
            stdout.execute(MoveTo(0, render_row))?;
            write!(
                stdout,
                "{}",
                Clear(crossterm::terminal::ClearType::CurrentLine)
            )?;

            let path = &self.matches[i];
            let mut display = super::display_safe(&path.to_string_lossy()).into_owned();
            let directory = if let Some(workspace) = &self.walk_workspace {
                workspace.validate().is_ok() && workspace.root().join(path).is_dir()
            } else {
                Path::new(&path).is_dir()
            };
            if directory {
                display.push('/');
            }
            let truncated =
                crate::ui::utils::display_prefix(&display, cols.saturating_sub(3) as usize);

            if i == self.selected {
                write!(stdout, "{}", SetForegroundColor(self.color(Color::Green)))?;
                write!(stdout, "▸ {}", truncated)?;
            } else {
                write!(
                    stdout,
                    "{}",
                    SetForegroundColor(self.color(Color::DarkGrey))
                )?;
                write!(stdout, "  {}", truncated)?;
            }
            write!(stdout, "{}", ResetColor)?;
        }
        stdout.flush()?;
        Ok(())
    }
}

/// Whether a walk entry is a dot-entry (`.git`, `.env`, `.cache`, ...). The
/// walk root itself (depth 0) is never pruned, even when its own name starts
/// with a dot.
fn is_dot_entry(entry: &ignore::DirEntry) -> bool {
    entry.depth() > 0 && entry.file_name().to_string_lossy().starts_with('.')
}

/// Walk `root`, invoking `emit` with batches of paths as they are found so
/// the picker can show matches incrementally. Stops early when `cancel` is
/// set (Esc/deactivate) or when `emit` returns false (receiver dropped).
///
/// The walk honours `.gitignore`/`.ignore`, prunes dot-directories such as
/// `.git` before descending into them, never emits the root itself, lists
/// directories alongside files, and stops after [`MAX_WALK_FILES`] files
/// (directories do not count toward that cap).
pub(crate) fn walk_files_streaming(
    root: &str,
    cancel: &AtomicBool,
    emit: impl FnMut(Vec<PathBuf>) -> bool,
) {
    walk_files_streaming_path(Path::new(root), cancel, emit);
}

fn walk_files_streaming_path(
    root: &Path,
    cancel: &AtomicBool,
    mut emit: impl FnMut(Vec<PathBuf>) -> bool,
) {
    let walker = ignore::WalkBuilder::new(root)
        // Dot-entries are pruned by `filter_entry` below so the walker never
        // descends into `.git`; the built-in hidden filter is equivalent but
        // implicit.
        .hidden(false)
        .git_ignore(true)
        .max_depth(Some(MAX_WALK_DEPTH))
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(|entry| !is_dot_entry(entry))
        .build();

    let mut batch = Vec::with_capacity(WALK_BATCH_SIZE);
    let mut files = 0usize;
    let mut entries = 0usize;
    for entry in walker.flatten() {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        if entry.depth() == 0 {
            continue;
        }
        let path = entry.path();
        let is_file = match entry.file_type() {
            Some(kind) if kind.is_file() => true,
            Some(kind) if kind.is_dir() => false,
            // Symlinks and unknown kinds: follow them once to classify.
            _ if path.is_file() => true,
            _ if path.is_dir() => false,
            _ => continue,
        };
        let rel = path.strip_prefix(root).unwrap_or(path);
        let rel = rel
            .to_string_lossy()
            .trim_start_matches(['/', '\\'])
            .to_string();
        if rel.is_empty() {
            continue;
        }
        batch.push(PathBuf::from(rel));
        entries += 1;
        if is_file {
            files += 1;
        }
        if batch.len() >= WALK_BATCH_SIZE && !emit(std::mem::take(&mut batch)) {
            return;
        }
        if files >= MAX_WALK_FILES || entries >= MAX_WALK_ENTRIES {
            break;
        }
    }
    if !batch.is_empty() {
        let _ = emit(batch);
    }
}

/// Collect the full walk into a `Vec` (used by the synchronous fallback and
/// tests).
pub(crate) fn walk_files(root: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    walk_files_streaming(root, &AtomicBool::new(false), |batch| {
        files.extend(batch);
        true
    });
    files
}

#[cfg(test)]
mod captured_add_tests {
    use super::*;
    #[tokio::test]
    async fn busy67_finished_add_picker_rejects_replaced_root_before_enter() {
        let outer = crate::tests::OwnedTestDirectory::new("add67-finished-root-replaced");
        let root = outer.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("old-name.txt"), "inert").unwrap();
        let binding = Arc::new(crate::paths::WorkspaceBinding::capture(&root).unwrap());
        let mut picker = FilePicker::new();
        picker.activate_for_add(Some(binding));
        picker.set_add_argument(5, "old");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while picker.is_loading() {
                picker.try_finish_loading();
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned walk must finish before replacement");
        assert!(picker.active);
        assert_eq!(picker.selected_path(), Some(&PathBuf::from("old-name.txt")));
        std::fs::rename(&root, outer.path().join("original")).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("replacement-name.txt"), "untrusted").unwrap();

        let mut draft: compact_str::CompactString = "/add old".into();
        let mut cursor = draft.len();
        assert!(super::super::handlers::handle_file_key(
            &mut draft,
            &mut cursor,
            &mut picker,
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        assert_eq!(draft.as_str(), "/add old");
        assert_eq!(cursor, draft.len());
        assert!(!picker.active && !picker.is_loading());
        // Finished pickers also revoke cached matches during a normal poll.
        assert!(picker.try_finish_loading());
        assert!(picker.matches.is_empty() && picker.file_cache.is_empty());
        assert!(
            !picker.try_finish_loading(),
            "retirement is reported only once"
        );
    }

    #[tokio::test]
    async fn busy67_add_walk_drops_results_when_captured_root_is_replaced() {
        let outer = crate::tests::OwnedTestDirectory::new("add67-root-replaced");
        let root = outer.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("old-name.txt"), "inert").unwrap();
        let binding = Arc::new(crate::paths::WorkspaceBinding::capture(&root).unwrap());
        let mut picker = FilePicker::new();
        picker.activate_for_add(Some(binding));
        picker.set_add_argument(5, "");
        std::fs::rename(&root, outer.path().join("original")).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("replacement-name.txt"), "untrusted").unwrap();
        assert!(picker.try_finish_loading());
        assert!(!picker.active && !picker.is_loading() && picker.matches.is_empty());
    }
}
