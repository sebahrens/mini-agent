use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

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

/// Matches kept per query; the overlay draws at most ten of them.
const MAX_MATCHES: usize = 50;

/// One walked (or virtual) entry. The lowercase form and the directory flag
/// are computed once, in the walk thread, so filtering is an allocation-free
/// `contains` and drawing never touches the filesystem.
pub(crate) struct CacheEntry {
    path: PathBuf,
    lower: String,
    is_dir: bool,
}

impl CacheEntry {
    fn new(path: PathBuf, is_dir: bool) -> Self {
        let lower = path.to_string_lossy().to_lowercase();
        CacheEntry {
            path,
            lower,
            is_dir,
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only counters: (cache entries examined by filtering, full rescans).
    static FILTER_COUNTS: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

#[cfg(test)]
pub(crate) fn take_filter_counts() -> (usize, usize) {
    FILTER_COUNTS.with(|counts| counts.replace((0, 0)))
}

#[cfg(test)]
fn count_filter(scanned: usize, rescan: bool) {
    FILTER_COUNTS.with(|counts| {
        let (entries, rescans) = counts.get();
        counts.set((entries + scanned, rescans + usize::from(rescan)));
    });
}

pub struct FilePicker {
    pub active: bool,
    pub query: String,
    pub cursor: usize,
    pub matches: Vec<PathBuf>,
    /// Whether each entry of `matches` is a directory (drawn with a `/`).
    match_dirs: Vec<bool>,
    pub selected: usize,
    pub(crate) argument_start: Option<usize>,
    file_cache: Vec<CacheEntry>,
    /// Lowercase query the current `matches` were computed for.
    query_lower: String,
    monochrome: bool,
    loading: bool,
    walk_rx: Option<mpsc::Receiver<Vec<CacheEntry>>>,
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
            match_dirs: Vec::new(),
            selected: 0,
            argument_start: None,
            file_cache: Vec::new(),
            query_lower: String::new(),
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
        self.file_cache
            .extend(items.map(|path| CacheEntry::new(path, false)));
        self.filter();
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
        self.match_dirs.clear();
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
                        walk_entries_streaming(workspace.root(), &cancel, |batch| {
                            workspace.validate().is_ok() && tx.send(batch).is_ok()
                        });
                    }
                } else {
                    walk_entries_streaming(Path::new("."), &cancel, |batch| tx.send(batch).is_ok());
                }
            });
        } else {
            self.load_files_sync();
        }
    }

    fn load_files_sync(&mut self) {
        if let Some(workspace) = &self.walk_workspace {
            let mut files = Vec::new();
            walk_entries_streaming(workspace.root(), &AtomicBool::new(false), |batch| {
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
            walk_entries_streaming(Path::new("."), &AtomicBool::new(false), |batch| {
                self.file_cache.extend(batch);
                true
            });
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
            self.match_dirs.clear();
            self.deactivate();
            return changed;
        }
        if !self.loading {
            return false;
        }
        let mut changed = false;
        let mut done = false;
        let first_new = self.file_cache.len();
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
        if changed {
            // Only the new entries can add matches: everything before them
            // was already scanned for this query.
            self.filter_from(first_new);
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

    /// Rescan the whole cache for the current query (the query changed).
    fn filter(&mut self) {
        self.query_lower = self.query.to_lowercase();
        self.matches.clear();
        self.match_dirs.clear();
        self.selected = 0;
        #[cfg(test)]
        count_filter(0, true);
        self.filter_from(0);
    }

    /// Append matches for the query from `file_cache[start..]` until
    /// [`MAX_MATCHES`] are kept. Matches stay in cache (walk) order, so
    /// filtering a streamed batch gives the same result as a full rescan.
    fn filter_from(&mut self, start: usize) {
        if self.matches.len() >= MAX_MATCHES {
            return;
        }
        let query = self.query_lower.as_str();
        for entry in self.file_cache.iter().skip(start) {
            #[cfg(test)]
            count_filter(1, false);
            if entry.lower.contains(query) {
                self.matches.push(entry.path.clone());
                self.match_dirs.push(entry.is_dir);
                if self.matches.len() >= MAX_MATCHES {
                    break;
                }
            }
        }
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

    /// Seed the cache directly. Directories are classified here, once,
    /// against the bound workspace (or the process directory).
    #[cfg(test)]
    pub fn test_set_cache(&mut self, files: Vec<PathBuf>) {
        let workspace = self.walk_workspace.clone();
        self.file_cache = files
            .into_iter()
            .map(|path| {
                let is_dir = match &workspace {
                    Some(workspace) => workspace.root().join(&path).is_dir(),
                    None => path.is_dir(),
                };
                CacheEntry::new(path, is_dir)
            })
            .collect();
        self.loading = false;
    }

    /// Draw the overlay from cached state only: background results are
    /// taken by `try_finish_loading` (polled by the event loop), and the
    /// directory marker comes from the walk, so drawing performs no
    /// filesystem calls.
    pub fn draw(&self, out: &mut dyn Write, cols: u16, floor_row: u16) -> std::io::Result<()> {
        if !self.active {
            return Ok(());
        }
        if self.loading && self.matches.is_empty() {
            return super::draw_picker_message(
                out,
                "scanning files...",
                self.monochrome,
                floor_row,
            );
        }
        super::draw_picker_rows(
            out,
            cols,
            self.matches.len(),
            |index| {
                let mut display = self.matches[index].to_string_lossy().into_owned();
                if self.match_dirs[index] {
                    display.push('/');
                }
                display.into()
            },
            self.selected,
            self.monochrome,
            None,
            floor_row,
            0,
        )
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
#[cfg(test)]
pub(crate) fn walk_files_streaming(
    root: &str,
    cancel: &AtomicBool,
    mut emit: impl FnMut(Vec<PathBuf>) -> bool,
) {
    walk_entries_streaming(Path::new(root), cancel, |batch| {
        emit(batch.into_iter().map(|entry| entry.path).collect())
    });
}

/// The walk behind [`walk_files_streaming`], emitting picker cache entries
/// whose lowercase text and directory flag are computed here, off the UI
/// task.
fn walk_entries_streaming(
    root: &Path,
    cancel: &AtomicBool,
    mut emit: impl FnMut(Vec<CacheEntry>) -> bool,
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
        batch.push(CacheEntry::new(PathBuf::from(rel), !is_file));
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
#[cfg(test)]
pub(crate) fn walk_files(root: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    walk_files_streaming(root, &AtomicBool::new(false), |batch| {
        files.extend(batch);
        true
    });
    files
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    fn naive_matches(paths: &[PathBuf], query: &str) -> Vec<PathBuf> {
        let query = query.to_lowercase();
        paths
            .iter()
            .filter(|path| path.to_string_lossy().to_lowercase().contains(&query))
            .take(MAX_MATCHES)
            .cloned()
            .collect()
    }

    #[test]
    fn char_input_on_a_20k_cache_scans_once_and_matches_a_naive_filter() {
        let paths: Vec<PathBuf> = (0..MAX_WALK_FILES)
            .map(|index| PathBuf::from(format!("Dir{}/File_{index}.RS", index % 37)))
            .collect();
        let mut picker = FilePicker::new();
        picker.test_set_cache(paths.clone());
        take_filter_counts();
        picker.char_input('e');
        picker.char_input('_');
        picker.char_input('1');
        assert_eq!(picker.matches, naive_matches(&paths, "e_1"));
        assert_eq!(picker.matches.len(), MAX_MATCHES);
        let (scanned, rescans) = take_filter_counts();
        assert_eq!(rescans, 3, "one rescan per query change");
        // Each rescan stops at the 50th match instead of lowercasing the
        // whole cache.
        assert!(scanned < 3 * 2_000, "scanned {scanned} entries");
        picker.char_input('9');
        picker.char_input('9');
        picker.char_input('9');
        assert_eq!(picker.matches, naive_matches(&paths, "e_1999"));
    }

    #[tokio::test]
    async fn streamed_batches_filter_only_new_entries_with_the_rescan_result() {
        let mut picker = FilePicker::new();
        picker.active = true;
        picker.query = "b".into();
        picker.filter();
        let (tx, rx) = mpsc::channel();
        picker.walk_rx = Some(rx);
        picker.loading = true;
        let mut all = Vec::new();
        take_filter_counts();
        for batch in 0..40 {
            let paths: Vec<PathBuf> = (0..WALK_BATCH_SIZE)
                .map(|index| PathBuf::from(format!("{}{batch}-{index}", ["a", "b"][index % 2])))
                .collect();
            all.extend(paths.clone());
            tx.send(
                paths
                    .into_iter()
                    .map(|path| CacheEntry::new(path, false))
                    .collect(),
            )
            .unwrap();
            assert!(picker.try_finish_loading());
        }
        drop(tx);
        assert!(picker.try_finish_loading());
        assert!(!picker.is_loading());
        assert_eq!(picker.matches, naive_matches(&all, "b"));
        let (scanned, rescans) = take_filter_counts();
        assert_eq!(rescans, 0, "streaming never rescans the cache");
        // Batches after the 50th match are not scanned at all.
        assert_eq!(scanned, 4 * WALK_BATCH_SIZE + 4);
    }

    #[test]
    fn draw_uses_the_cached_directory_flag_without_touching_the_filesystem() {
        let root = crate::tests::OwnedTestDirectory::new("file-picker-draw-cache");
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/main.rs"), "").unwrap();
        let binding = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut picker = FilePicker::new();
        picker.active = true;
        picker.walk_workspace = Some(binding.clone());
        walk_entries_streaming(binding.root(), &AtomicBool::new(false), |batch| {
            picker.file_cache.extend(batch);
            true
        });
        picker.filter();
        assert_eq!(picker.match_dirs, vec![true, false]);
        drop(binding);
        drop(root);
        let mut bytes = Vec::new();
        picker.draw(&mut bytes, 80, 20).unwrap();
        let rendered = String::from_utf8(bytes).unwrap();
        assert!(rendered.contains("src/"), "{rendered:?}");
        assert!(rendered.contains("src/main.rs"));
        assert!(!rendered.contains("main.rs/"));
    }
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
