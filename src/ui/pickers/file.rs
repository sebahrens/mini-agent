use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use crossterm::style::Color;

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

/// A finished walk up to this old is shown at once while a background walk
/// revalidates it; an older one is dropped and the picker walks from scratch.
const WALK_REUSE_STALE: std::time::Duration = std::time::Duration::from_secs(600);
/// A cached walk younger than this is shown as is, without a background
/// re-walk: files created in the last couple of seconds appear on the next
/// activation after the window.
const WALK_REVALIDATE_MIN: std::time::Duration = std::time::Duration::from_secs(2);
/// Batches between workspace-binding checks inside a walk (40 x 25 = 1000
/// entries). The first batch and the end of the walk are always checked.
const VALIDATE_EVERY_BATCHES: usize = 40;
/// Walk results kept, one per root.
const WALK_SNAPSHOTS: usize = 4;

/// The last completed walk of a root: names only, no filesystem authority.
struct WalkSnapshot {
    root: PathBuf,
    finished: std::time::Instant,
    entries: Vec<CacheEntry>,
}

/// Roots with a walk running, so a repeated press revalidates only once.
static WALKS_IN_FLIGHT: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

fn walk_in_flight(root: &Path) -> bool {
    WALKS_IN_FLIGHT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .any(|running| running == root)
}

/// Marks a root as walking until dropped.
struct InFlight(PathBuf);
impl InFlight {
    fn begin(root: &Path) -> Self {
        WALKS_IN_FLIGHT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(root.to_path_buf());
        InFlight(root.to_path_buf())
    }
}
impl Drop for InFlight {
    fn drop(&mut self) {
        let mut running = WALKS_IN_FLIGHT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(at) = running.iter().position(|root| *root == self.0) {
            running.remove(at);
        }
    }
}

/// A closed/open flag plus the condition variable that wakes a waiter when
/// it changes.
#[cfg(test)]
type WalkGateState = Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>;

/// Test-only gates keyed by root, so a test can hold a specific root's
/// background walk closed without affecting any other root's walk, even one
/// racing concurrently in another test in the same binary.
#[cfg(test)]
static WALK_GATES: std::sync::Mutex<Vec<(PathBuf, WalkGateState)>> =
    std::sync::Mutex::new(Vec::new());

/// Test-only: blocks the next background walk(s) started for `root`
/// immediately after they start — before they touch the filesystem or
/// publish into [`WALK_SNAPSHOT_CACHE`] — until this guard drops. Lets a
/// test observe the shared cache and `WALKS_IN_FLIGHT` deterministically in
/// the window between a walk starting and it publishing a new snapshot,
/// instead of racing the real walk thread's completion (which, on a real OS
/// thread, can finish before the test's own synchronous code runs its next
/// assertion).
#[cfg(test)]
struct WalkGate(PathBuf);

#[cfg(test)]
impl WalkGate {
    fn close(root: &Path) -> Self {
        let state = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        WALK_GATES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((root.to_path_buf(), state));
        WalkGate(root.to_path_buf())
    }
}

#[cfg(test)]
impl Drop for WalkGate {
    fn drop(&mut self) {
        let mut gates = WALK_GATES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        gates.retain(|(root, state)| {
            if *root != self.0 {
                return true;
            }
            let (open, signal) = &**state;
            *open
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
            signal.notify_all();
            false
        });
    }
}

/// Blocks while a [`WalkGate`] for `root` is held; a no-op when none is.
/// Called by the background walk thread itself, right after it starts.
#[cfg(test)]
fn wait_for_test_gate(root: &Path) {
    let state = WALK_GATES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .find(|(gate_root, _)| gate_root == root)
        .map(|(_, state)| state.clone());
    let Some(state) = state else {
        return;
    };
    let (open, signal) = &*state;
    let mut open = open
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while !*open {
        open = signal
            .wait(open)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}

static WALK_SNAPSHOT_CACHE: std::sync::Mutex<Vec<WalkSnapshot>> = std::sync::Mutex::new(Vec::new());

/// The root a walk of `workspace` covers; the process directory without one.
fn walk_root(workspace: Option<&crate::paths::WorkspaceBinding>) -> PathBuf {
    match workspace {
        Some(workspace) => workspace.root().to_path_buf(),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}

/// The remembered walk of `root` and how long ago it finished.
fn cached_walk(root: &Path) -> Option<(Vec<CacheEntry>, std::time::Instant)> {
    let cache = WALK_SNAPSHOT_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let snapshot = cache.iter().find(|snapshot| snapshot.root == root)?;
    (snapshot.finished.elapsed() <= WALK_REUSE_STALE)
        .then(|| (snapshot.entries.clone(), snapshot.finished))
}

fn remember_walk(root: PathBuf, entries: Vec<CacheEntry>) {
    let mut cache = WALK_SNAPSHOT_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache.retain(|snapshot| {
        snapshot.root != root && snapshot.finished.elapsed() <= WALK_REUSE_STALE
    });
    if cache.len() >= WALK_SNAPSHOTS {
        cache.remove(0);
    }
    cache.push(WalkSnapshot {
        root,
        finished: std::time::Instant::now(),
        entries,
    });
}

/// Drops walk snapshots too old to be shown, releasing their lists.
fn evict_stale_walks() {
    WALK_SNAPSHOT_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|snapshot| snapshot.finished.elapsed() <= WALK_REUSE_STALE);
}

/// Test-only: pretend the remembered walk of `root` finished `by` earlier.
#[cfg(test)]
fn backdate_snapshot(root: &Path, by: std::time::Duration) {
    let mut cache = WALK_SNAPSHOT_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(snapshot) = cache.iter_mut().find(|snapshot| snapshot.root == root) {
        snapshot.finished -= by;
    }
}

/// One walked (or virtual) entry. The lowercase form and the directory flag
/// are computed once, in the walk thread, so filtering is an allocation-free
/// `contains` and drawing never touches the filesystem. Text is shared, so
/// cloning an entry (snapshot, picker, walk accumulator) is refcount bumps.
#[derive(Clone)]
pub(crate) struct CacheEntry {
    /// The path as shown (relative for walked entries).
    text: Arc<str>,
    /// Lowercase form; `None` when `text` is already lowercase.
    lower: Option<Arc<str>>,
    is_dir: bool,
    kind: EntryKind,
    /// Display-only detail drawn dimmed after a mention row (a persona's
    /// one-line description). Shared, so matching clones a pointer, and
    /// never part of `lower`, so filtering stays on the name.
    description: Option<Arc<str>>,
}

/// Where a cache entry came from. Virtual entries are completion strings
/// (they keep their own `@` marker) and never depend on the walk root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EntryKind {
    /// A walked filesystem path.
    Path,
    /// An opaque operand such as `@mcp:<server>:<uri>`, inserted verbatim.
    #[cfg_attr(not(feature = "mcp"), allow(dead_code))]
    Opaque,
    /// A mention such as `@agent:<name>`, inserted with a trailing space.
    #[cfg_attr(not(feature = "subagents"), allow(dead_code))]
    Mention,
}

impl CacheEntry {
    #[cfg(test)]
    fn new(path: PathBuf, is_dir: bool) -> Self {
        Self::with_kind(path, is_dir, EntryKind::Path)
    }

    #[cfg(any(test, feature = "mcp", feature = "subagents"))]
    fn with_kind(path: PathBuf, is_dir: bool, kind: EntryKind) -> Self {
        Self::from_text(Arc::from(path.to_string_lossy().as_ref()), is_dir, kind)
    }

    fn from_text(text: Arc<str>, is_dir: bool, kind: EntryKind) -> Self {
        let lowered = text.to_lowercase();
        let lower = (lowered != *text).then(|| Arc::from(lowered));
        CacheEntry {
            text,
            lower,
            is_dir,
            kind,
            description: None,
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
thread_local! {
    /// Test-only: directory walks started by pickers on this thread.
    static WALKS_STARTED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn take_walks_started() -> usize {
    WALKS_STARTED.with(|walks| walks.replace(0))
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
    /// The [`EntryKind`] of each entry of `matches`.
    match_kinds: Vec<EntryKind>,
    /// The display-only description of each entry of `matches`.
    match_descriptions: Vec<Option<Arc<str>>>,
    pub selected: usize,
    pub(crate) argument_start: Option<usize>,
    file_cache: Vec<CacheEntry>,
    /// Number of virtual entries at the front of `file_cache`. They precede
    /// every walked path, so they fill the first window for a short query.
    virtual_len: usize,
    /// Lowercase query the current `matches` were computed for.
    query_lower: String,
    monochrome: bool,
    accent: Option<Color>,
    loading: bool,
    walk_rx: Option<mpsc::Receiver<Vec<CacheEntry>>>,
    walk_cancel: Arc<AtomicBool>,
    walk_workspace: Option<Arc<crate::paths::WorkspaceBinding>>,
    /// Set by the walk thread when it ran to the end (not cancelled).
    walk_complete: Arc<AtomicBool>,
    /// While a remembered list is on screen, the background walk that will
    /// replace it collects here; it swaps in when the walk completes.
    refresh: Option<Vec<CacheEntry>>,
    /// Another picker's walk of this root is running: the root and the
    /// snapshot time shown now. A newer snapshot replaces the list.
    waiting: Option<(PathBuf, std::time::Instant)>,
}

impl FilePicker {
    pub fn new() -> Self {
        FilePicker {
            active: false,
            query: String::new(),
            cursor: 0,
            matches: Vec::new(),
            match_dirs: Vec::new(),
            match_kinds: Vec::new(),
            match_descriptions: Vec::new(),
            selected: 0,
            argument_start: None,
            file_cache: Vec::new(),
            virtual_len: 0,
            query_lower: String::new(),
            monochrome: false,
            accent: None,
            loading: false,
            walk_rx: None,
            walk_cancel: Arc::new(AtomicBool::new(false)),
            walk_workspace: None,
            walk_complete: Arc::new(AtomicBool::new(false)),
            refresh: None,
            waiting: None,
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

    pub fn set_accent_color(&mut self, accent: Option<Color>) {
        self.accent = accent;
    }

    /// Opaque MCP operands are completion strings, never filesystem walk roots.
    #[cfg(feature = "mcp")]
    pub(crate) fn append_virtual_items(&mut self, items: impl Iterator<Item = PathBuf>) {
        self.insert_virtual(items, EntryKind::Opaque);
    }

    #[cfg(feature = "mcp")]
    fn insert_virtual(&mut self, items: impl Iterator<Item = PathBuf>, kind: EntryKind) {
        self.insert_described_virtual(items.map(|path| (path, None)), kind);
    }

    /// `@agent:<name>` mention rows, inserted with a trailing space. The
    /// description is drawn dimmed after the row and never matched.
    #[cfg(feature = "subagents")]
    pub(crate) fn append_virtual_mentions(
        &mut self,
        items: impl Iterator<Item = (PathBuf, Arc<str>)>,
    ) {
        self.insert_described_virtual(
            items.map(|(path, description)| (path, Some(description))),
            EntryKind::Mention,
        );
    }

    /// Insert virtual rows after earlier virtual rows and before every walked
    /// path (a synchronous walk may already have filled the cache).
    #[cfg(any(feature = "mcp", feature = "subagents"))]
    fn insert_described_virtual(
        &mut self,
        items: impl Iterator<Item = (PathBuf, Option<Arc<str>>)>,
        kind: EntryKind,
    ) {
        let at = self.virtual_len;
        let entries: Vec<_> = items
            .map(|(path, description)| CacheEntry {
                description,
                ..CacheEntry::with_kind(path, false, kind)
            })
            .collect();
        self.virtual_len += entries.len();
        self.file_cache.splice(at..at, entries);
        self.filter();
    }

    /// Whether the highlighted row is a mention that completes with a space.
    pub(crate) fn selected_is_mention(&self) -> bool {
        self.match_kinds.get(self.selected) == Some(&EntryKind::Mention)
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
        self.match_kinds.clear();
        self.match_descriptions.clear();
        self.selected = 0;
        self.file_cache.clear();
        self.virtual_len = 0;
        self.loading = false;
        self.refresh = None;
        self.waiting = None;
        self.walk_workspace = workspace.clone();
        if require_binding
            && workspace
                .as_ref()
                .is_none_or(|root| root.validate().is_err())
        {
            return;
        }

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            // A walk of this root finished recently: show it at once and
            // revalidate it with a background walk, unless one is running.
            let root = walk_root(workspace.as_deref());
            if let Some((entries, finished)) = cached_walk(&root) {
                self.file_cache = entries;
                self.filter();
                if walk_in_flight(&root) {
                    self.waiting = Some((root, finished));
                    self.loading = true;
                    return;
                }
                if finished.elapsed() < WALK_REVALIDATE_MIN {
                    self.loading = false;
                    return;
                }
                self.refresh = Some(Vec::new());
            }
            #[cfg(test)]
            WALKS_STARTED.with(|walks| walks.set(walks.get() + 1));
            self.loading = true;
            self.walk_cancel = Arc::new(AtomicBool::new(false));
            self.walk_complete = Arc::new(AtomicBool::new(false));
            let cancel = self.walk_cancel.clone();
            let complete = self.walk_complete.clone();
            let (tx, rx) = mpsc::channel();
            self.walk_rx = Some(rx);
            let running = InFlight::begin(&root);
            #[cfg(test)]
            let gated_root = root.clone();
            handle.spawn_blocking(move || {
                let _running = running;
                #[cfg(test)]
                wait_for_test_gate(&gated_root);
                let mut all = Vec::new();
                let finished = if let Some(workspace) = workspace {
                    let mut batches = 0usize;
                    workspace.validate().is_ok()
                        && walk_entries_streaming(workspace.root(), &cancel, |batch| {
                            all.extend(batch.iter().cloned());
                            batches += 1;
                            (!batches.is_multiple_of(VALIDATE_EVERY_BATCHES) || workspace.validate().is_ok())
                                && tx.send(batch).is_ok()
                        })
                        // A replaced root is never remembered or completed.
                        && workspace.validate().is_ok()
                } else {
                    walk_entries_streaming(Path::new("."), &cancel, |batch| {
                        all.extend(batch.iter().cloned());
                        tx.send(batch).is_ok()
                    })
                };
                if finished {
                    remember_walk(root, all);
                    complete.store(true, Ordering::Release);
                }
            });
        } else {
            self.load_files_sync();
        }
    }

    fn load_files_sync(&mut self) {
        if let Some(workspace) = &self.walk_workspace {
            let mut files = Vec::new();
            let mut batches = 0usize;
            walk_entries_streaming(workspace.root(), &AtomicBool::new(false), |batch| {
                if batches.is_multiple_of(VALIDATE_EVERY_BATCHES) && workspace.validate().is_err() {
                    return false;
                }
                batches += 1;
                files.extend(batch);
                true
            });
            if workspace.validate().is_ok() {
                self.file_cache.truncate(self.virtual_len);
                self.file_cache.extend(files);
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
        self.refresh = None;
        self.waiting = None;
        evict_stale_walks();
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
            self.virtual_len = 0;
            self.matches.clear();
            self.match_dirs.clear();
            self.match_kinds.clear();
            self.match_descriptions.clear();
            self.deactivate();
            return changed;
        }
        if let Some((root, shown)) = self.waiting.clone() {
            let newer = cached_walk(&root).filter(|(_, finished)| *finished > shown);
            if let Some((entries, _)) = newer {
                self.waiting = None;
                self.loading = false;
                self.file_cache.truncate(self.virtual_len);
                self.file_cache.extend(entries);
                self.filter();
                return true;
            }
            if !walk_in_flight(&root) {
                self.waiting = None;
                self.loading = false;
                return true;
            }
            return false;
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
                        if let Some(fresh) = &mut self.refresh {
                            fresh.extend(batch);
                        } else {
                            self.file_cache.extend(batch);
                            changed = true;
                        }
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
            if let Some(fresh) = self.refresh.take()
                && self.walk_complete.load(Ordering::Acquire)
            {
                // The remembered list gives way to the finished walk.
                self.file_cache.truncate(self.virtual_len);
                self.file_cache.extend(fresh);
                self.filter();
                return true;
            }
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

    /// Insert `text` at the query cursor as one edit: the cache is scanned
    /// once for the result, which equals typing the characters one by one.
    pub(crate) fn push_query_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let byte_pos = self
            .query
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.query.len());
        self.query.insert_str(byte_pos, text);
        self.cursor += text.chars().count();
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
        self.match_kinds.clear();
        self.match_descriptions.clear();
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
            if entry
                .lower
                .as_deref()
                .unwrap_or(&entry.text)
                .contains(query)
            {
                self.matches.push(PathBuf::from(&*entry.text));
                self.match_dirs.push(entry.is_dir);
                self.match_kinds.push(entry.kind);
                self.match_descriptions.push(entry.description.clone());
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
        // Virtual rows are completion strings, not paths under the walk root,
        // so a stale workspace never hides them.
        if self
            .match_kinds
            .get(self.selected)
            .is_some_and(|kind| *kind != EntryKind::Path)
        {
            return self.matches.get(self.selected);
        }
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
        self.file_cache.truncate(self.virtual_len);
        self.file_cache.extend(files.into_iter().map(|path| {
            let is_dir = match &workspace {
                Some(workspace) => workspace.root().join(&path).is_dir(),
                None => path.is_dir(),
            };
            CacheEntry::new(path, is_dir)
        }));
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
        super::draw_picker_described_rows(
            out,
            cols,
            self.matches.len(),
            |index| {
                let mut display = self.matches[index].to_string_lossy().into_owned();
                if self.match_dirs[index] {
                    display.push('/');
                }
                (display.into(), self.match_descriptions[index].as_deref())
            },
            self.selected,
            self.monochrome,
            self.accent,
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
    let _ = walk_entries_streaming(Path::new(root), cancel, |batch| {
        emit(
            batch
                .into_iter()
                .map(|entry| PathBuf::from(&*entry.text))
                .collect(),
        )
    });
}

/// The walk behind [`walk_files_streaming`], emitting picker cache entries
/// whose lowercase text and directory flag are computed here, off the UI
/// task.
fn walk_entries_streaming(
    root: &Path,
    cancel: &AtomicBool,
    mut emit: impl FnMut(Vec<CacheEntry>) -> bool,
) -> bool {
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
            return false;
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
        let rel = rel.to_string_lossy();
        let rel = rel.trim_start_matches(['/', '\\']);
        if rel.is_empty() {
            continue;
        }
        batch.push(CacheEntry::from_text(
            Arc::from(rel),
            !is_file,
            EntryKind::Path,
        ));
        entries += 1;
        if is_file {
            files += 1;
        }
        if batch.len() >= WALK_BATCH_SIZE && !emit(std::mem::take(&mut batch)) {
            return false;
        }
        if files >= MAX_WALK_FILES || entries >= MAX_WALK_ENTRIES {
            break;
        }
    }
    batch.is_empty() || emit(batch)
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

    async fn finish_walk(picker: &mut FilePicker) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while picker.is_loading() {
                picker.try_finish_loading();
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("walk finishes");
    }

    #[test]
    fn a_pasted_query_scans_the_cache_once_and_matches_typing_it() {
        let paths: Vec<PathBuf> = (0..1_000)
            .map(|index| PathBuf::from(format!("Dir{}/File_{index}.RS", index % 37)))
            .collect();
        let pasted = "dir3/file_3";
        let mut typed = FilePicker::new();
        typed.test_set_cache(paths.clone());
        for c in pasted.chars() {
            typed.char_input(c);
        }
        let mut picker = FilePicker::new();
        picker.test_set_cache(paths);
        take_filter_counts();
        picker.push_query_str("dir3/");
        picker.push_query_str(&pasted["dir3/".len()..]);
        let (_, rescans) = take_filter_counts();
        assert_eq!(rescans, 2, "one rescan per pasted segment");
        assert_eq!(picker.query, typed.query);
        assert_eq!(picker.cursor, typed.cursor);
        assert_eq!(picker.matches, typed.matches);
        assert!(!picker.matches.is_empty());
    }

    #[tokio::test]
    async fn every_press_shows_the_cached_walk_and_revalidates_it_once() {
        let root = crate::tests::OwnedTestDirectory::new("file-picker-walk-reuse");
        std::fs::write(root.path().join("first.txt"), "").unwrap();
        let binding = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        take_walks_started();
        let mut first = FilePicker::new();
        first.activate_for_add(Some(binding.clone()));
        finish_walk(&mut first).await;
        assert_eq!(take_walks_started(), 1);

        std::fs::write(root.path().join("second.txt"), "").unwrap();
        // Inside the revalidation window a press only shows the snapshot.
        let mut young = FilePicker::new();
        young.activate_for_add(Some(binding.clone()));
        assert_eq!(take_walks_started(), 0, "young snapshot starts no walk");
        assert!(!young.is_loading());
        backdate_snapshot(binding.root(), WALK_REVALIDATE_MIN * 2);
        // `second`'s background walk is real OS-thread concurrency: on a
        // loaded machine it could publish its snapshot before the
        // "still shows the stale cache" assertions below. Close the gate for
        // this root so that walk blocks right after it starts, keeping it in
        // flight until both presses observed it.
        let gate = WalkGate::close(binding.root());
        // The next press shows the cached list at once and starts exactly
        // one background walk.
        let mut second = FilePicker::new();
        second.activate_for_add(Some(binding.clone()));
        assert_eq!(take_walks_started(), 1);
        assert!(second.is_loading());
        second.set_add_argument(5, ".txt");
        assert_eq!(second.matches, vec![PathBuf::from("first.txt")]);
        // A press while that walk is in flight shows the cache and starts
        // no other walk.
        let mut third = FilePicker::new();
        third.activate_for_add(Some(binding));
        assert_eq!(take_walks_started(), 0);
        assert!(third.is_loading());
        third.set_add_argument(5, ".txt");
        assert_eq!(third.matches, vec![PathBuf::from("first.txt")]);
        drop(gate);
        // When the walk finishes, both open pickers pick up the new file.
        finish_walk(&mut second).await;
        finish_walk(&mut third).await;
        for picker in [&mut second, &mut third] {
            picker.set_add_argument(5, "second");
            assert_eq!(picker.matches, vec![PathBuf::from("second.txt")]);
        }
    }

    #[tokio::test]
    async fn a_large_add_walk_validates_the_workspace_rarely_and_shares_entry_text() {
        let root = crate::tests::OwnedTestDirectory::new("file-picker-validate-rarely");
        for index in 0..2_000 {
            std::fs::write(root.path().join(format!("f{index}.txt")), "").unwrap();
        }
        let binding = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let before = binding.validation_count();
        let mut picker = FilePicker::new();
        picker.activate_for_add(Some(binding.clone()));
        // Wait on the walk thread's completion flag, not on UI polls (each
        // poll validates the binding by design).
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !picker.walk_complete.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("walk finishes");
        // activate + walk start + one per 1000 entries + the end.
        let walk_validates = binding.validation_count() - before;
        assert!(walk_validates <= 6, "validated {walk_validates} times");
        finish_walk(&mut picker).await;
        let (snapshot, _) = cached_walk(binding.root()).expect("walk remembered");
        assert_eq!(snapshot.len(), 2_000);
        let mut second = FilePicker::new();
        second.file_cache = snapshot.clone();
        assert!(Arc::ptr_eq(&snapshot[0].text, &second.file_cache[0].text));
    }

    #[test]
    fn stale_snapshots_are_evicted_when_another_root_is_remembered() {
        let stale = PathBuf::from("/nonexistent/stale-root-vb5vw33");
        let fresh = PathBuf::from("/nonexistent/fresh-root-vb5vw33");
        remember_walk(
            stale.clone(),
            vec![CacheEntry::new(PathBuf::from("a"), false)],
        );
        backdate_snapshot(&stale, WALK_REUSE_STALE * 2);
        remember_walk(fresh.clone(), Vec::new());
        let cache = WALK_SNAPSHOT_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(cache.iter().all(|snapshot| snapshot.root != stale));
        assert!(cache.iter().any(|snapshot| snapshot.root == fresh));
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
