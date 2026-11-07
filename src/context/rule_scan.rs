//! Deterministic, reporting discovery of `.claude/rules/**/*.md` files.
//!
//! The walk reads every entry name of a directory (bounded by a hard limit of
//! sixteen times the entry cap), sorts them and only then applies the entry
//! cap, so which rules survive a cap never depends on `read_dir` order. A
//! directory larger than the hard limit is reported as order dependent.
//! Anything the walk
//! cannot honour (file-count, depth or entry caps, links it refuses) is
//! returned as a note naming the item and the limit, so the caller can show it
//! instead of silently dropping the rule.
//!
//! Symlinked rule files and directories (and a symlinked rules root such as a
//! stow-managed `~/.claude/rules`) are followed when the canonical target is
//! inside the workspace (never `.git`), or inside the user's `~/.claude/rules`
//! (only with `user_instruction_files`), or, with the opt-in
//! `rule_external_symlinks`, inside the user's home directory; the target is
//! opened through a capability on its real location, never through the link.
//! Every refused link or root is reported as a note.
use crate::paths::WorkspaceBinding;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Notes kept per scan; further ones are summarised in a final count.
const MAX_NOTES: usize = 24;

/// Where a rule file is physically read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RuleOrigin {
    /// The workspace binding.
    Workspace,
    /// The `~/.claude` binding (user-level rules).
    User,
    /// A link target outside both, inside the opted-in external root; read
    /// through [`RuleFile::external`].
    External,
}

/// One discovered rule file.
#[derive(Clone, Debug)]
pub(crate) struct RuleFile {
    /// The spelled path used for display and labels
    /// (`.claude/rules/a.md`, `rules/a.md` for user rules).
    pub(crate) display: PathBuf,
    pub(crate) origin: RuleOrigin,
    /// The link-free path under the origin's binding the file is read from.
    pub(crate) real: PathBuf,
    /// The capability `real` is relative to, for [`RuleOrigin::External`].
    pub(crate) external: Option<Arc<WorkspaceBinding>>,
}

impl RuleFile {
    /// What identifies the physical file: the same one reached through two
    /// links loads once.
    pub(crate) fn identity(&self) -> (RuleOrigin, PathBuf) {
        match &self.external {
            Some(binding) => (self.origin, binding.root().join(&self.real)),
            None => (self.origin, self.real.clone()),
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct RuleScan {
    pub(crate) files: Vec<RuleFile>,
    pub(crate) notes: Vec<String>,
}

/// The user's `~/.claude` capability, for `rules/` below it.
#[derive(Clone)]
pub(crate) struct UserClaudeDir {
    pub(crate) binding: Arc<WorkspaceBinding>,
}

impl UserClaudeDir {
    /// `~/.claude` under `home`, or `None` when it is absent or unusable.
    pub(crate) fn capture(home: &Path) -> Option<Self> {
        let binding = WorkspaceBinding::capture(&home.join(".claude")).ok()?;
        Some(Self {
            binding: Arc::new(binding),
        })
    }
}

/// Limits for one scan.
#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub(crate) depth: usize,
    pub(crate) entries: usize,
    pub(crate) files: usize,
}

struct Walk<'a> {
    workspace: &'a WorkspaceBinding,
    user: Option<&'a UserClaudeDir>,
    external_root: Option<&'a Path>,
    limits: Limits,
    scanned: usize,
    visited: HashSet<(RuleOrigin, PathBuf)>,
    files: Vec<RuleFile>,
    notes: Vec<String>,
    dropped_notes: usize,
}

struct Item {
    origin: RuleOrigin,
    real: PathBuf,
    display: PathBuf,
    depth: usize,
    external: Option<Arc<WorkspaceBinding>>,
}

/// Scans `rules_dir` (workspace relative, e.g. `.claude/rules`) and, when
/// `user` is given, the user's `rules` directory with the same limits.
/// `external_root` is the directory outside the workspace that links may
/// resolve into (the opt-in `rule_external_symlinks`); `None` keeps it closed.
pub(crate) fn scan(
    workspace: &WorkspaceBinding,
    rules_dir: &Path,
    user: Option<&UserClaudeDir>,
    external_root: Option<&Path>,
    limits: Limits,
) -> (RuleScan, RuleScan) {
    let project = scan_unit(
        workspace,
        user,
        external_root,
        limits,
        RuleOrigin::Workspace,
        rules_dir,
    );
    let user_scan = match user {
        Some(user) => scan_unit(
            workspace,
            Some(user),
            external_root,
            limits,
            RuleOrigin::User,
            Path::new("rules"),
        ),
        None => RuleScan::default(),
    };
    (project, user_scan)
}

fn scan_unit(
    workspace: &WorkspaceBinding,
    user: Option<&UserClaudeDir>,
    external_root: Option<&Path>,
    limits: Limits,
    origin: RuleOrigin,
    base: &Path,
) -> RuleScan {
    let mut walk = Walk {
        workspace,
        user,
        external_root,
        limits,
        scanned: 0,
        visited: HashSet::new(),
        files: Vec::new(),
        notes: Vec::new(),
        dropped_notes: 0,
    };
    walk.visited.insert((origin, base.to_path_buf()));
    let root = walk.root_item(origin, base);
    let mut pending: Vec<Item> = root.into_iter().collect();
    while let Some(item) = pending.pop() {
        walk.visit(item, &mut pending);
    }
    walk.finish(origin, base)
}

impl Walk<'_> {
    fn binding<'b>(
        &'b self,
        origin: RuleOrigin,
        external: Option<&'b Arc<WorkspaceBinding>>,
    ) -> Option<&'b WorkspaceBinding> {
        match origin {
            RuleOrigin::Workspace => Some(self.workspace),
            RuleOrigin::User => self.user.map(|user| &*user.binding),
            RuleOrigin::External => external.map(|binding| &**binding),
        }
    }

    /// The key a directory is visited at most once under.
    fn key(
        &self,
        origin: RuleOrigin,
        external: Option<&Arc<WorkspaceBinding>>,
        real: &Path,
    ) -> (RuleOrigin, PathBuf) {
        match external {
            Some(binding) => (origin, binding.root().join(real)),
            None => (origin, real.to_path_buf()),
        }
    }

    /// The starting directory of a scan. When the rules root itself is a
    /// symlink it is resolved under the same containment policy as any other
    /// link and reported when refused; a missing root is silent.
    fn root_item(&mut self, origin: RuleOrigin, base: &Path) -> Option<Item> {
        let direct = Item {
            origin,
            real: base.to_path_buf(),
            display: base.to_path_buf(),
            depth: 0,
            external: None,
        };
        let binding = self.binding(origin, None)?;
        if binding.open_dir_relative(base).is_ok() {
            return Some(direct);
        }
        let disk = self.disk_path(origin, None, base);
        let shown = match origin {
            RuleOrigin::User => format!("~/.claude/{}", base.display()),
            _ => base.display().to_string(),
        };
        let metadata = std::fs::symlink_metadata(&disk).ok()?;
        if !metadata.file_type().is_symlink() {
            // Some other component is a link, or the path is not a directory.
            if metadata.is_dir() {
                self.note(format!(
                    "rule directory {shown} could not be opened; its rules were not loaded"
                ));
            }
            return None;
        }
        match self.resolve(origin, None, base) {
            Some((target_origin, real, external)) => {
                let is_dir =
                    std::fs::metadata(self.disk_path(target_origin, external.as_ref(), &real))
                        .is_ok_and(|m| m.is_dir());
                if !is_dir {
                    self.note(format!(
                        "rule directory {shown} is a symlink to something that is not a directory"
                    ));
                    return None;
                }
                self.visited
                    .insert(self.key(target_origin, external.as_ref(), &real));
                Some(Item {
                    origin: target_origin,
                    real,
                    display: base.to_path_buf(),
                    depth: 0,
                    external,
                })
            }
            None => {
                self.note(format!(
                    "rule directory {shown} is a symlink and was not followed: {}",
                    self.refusal()
                ));
                None
            }
        }
    }

    /// Why a link target was refused, naming the opt-ins that would widen it.
    fn refusal(&self) -> &'static str {
        if self.external_root.is_some() {
            "its target is outside the workspace, ~/.claude/rules and the home directory \
             (or unreadable, or inside .git)"
        } else {
            "its target is outside the workspace and ~/.claude/rules (or unreadable); \
             rule_external_symlinks = true allows targets inside the home directory"
        }
    }

    fn note(&mut self, text: String) {
        tracing::warn!("{text}");
        if self.notes.len() < MAX_NOTES {
            self.notes.push(text);
        } else {
            self.dropped_notes += 1;
        }
    }

    fn visit(&mut self, item: Item, pending: &mut Vec<Item>) {
        let Some(binding) = self.binding(item.origin, item.external.as_ref()) else {
            return;
        };
        let Ok(directory) = binding.open_dir_relative(&item.real) else {
            return;
        };
        let Ok(entries) = directory.read_dir(".") else {
            return;
        };
        // Names first, sorted, and only then the cap: which entries survive
        // never depends on `read_dir` order. The hard limit bounds memory.
        let hard_limit = self.limits.entries.saturating_mul(16).max(1);
        let mut found = Vec::new();
        let mut unsorted_tail = false;
        for entry in entries {
            if found.len() >= hard_limit {
                unsorted_tail = true;
                break;
            }
            let Ok(entry) = entry else { continue };
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            found.push((entry.file_name(), kind));
        }
        found.sort_by(|a, b| a.0.cmp(&b.0));
        let allowed = self.limits.entries.saturating_sub(self.scanned);
        if found.len() > allowed {
            let first_dropped = found[allowed].0.to_string_lossy().into_owned();
            found.truncate(allowed);
            self.note(format!(
                "rule scan stopped after {} directory entries (limit) while reading {}; \
                 entries in sorted order from {first_dropped} onwards were not loaded: \
                 split or prune the directory",
                self.limits.entries,
                item.display.display()
            ));
        }
        self.scanned += found.len();
        if unsorted_tail {
            self.note(format!(
                "{} has more than {hard_limit} entries; only the first {hard_limit} in \
                 directory order were considered, so which rules loaded may depend on \
                 directory order: split or prune the directory",
                item.display.display()
            ));
        }
        let mut children = Vec::new();
        for (name, kind) in found {
            let display = item.display.join(&name);
            let real = item.real.join(&name);
            if kind.is_dir() {
                if item.depth + 1 < self.limits.depth {
                    if self
                        .visited
                        .insert(self.key(item.origin, item.external.as_ref(), &real))
                    {
                        children.push(Item {
                            origin: item.origin,
                            real,
                            display,
                            depth: item.depth + 1,
                            external: item.external.clone(),
                        });
                    }
                } else {
                    self.note(format!(
                        "rule directory {} not scanned: deeper than {} levels (limit)",
                        display.display(),
                        self.limits.depth
                    ));
                }
            } else if kind.is_file() {
                if is_markdown(&name) {
                    self.files.push(RuleFile {
                        display,
                        origin: item.origin,
                        real,
                        external: item.external.clone(),
                    });
                }
            } else if kind.is_symlink() {
                self.link(&item, display, &real, item.depth, &mut children);
            }
        }
        // Popped in reverse, so sorted order is the visit order.
        pending.extend(children.into_iter().rev());
    }

    fn link(
        &mut self,
        item: &Item,
        display: PathBuf,
        real: &Path,
        depth: usize,
        children: &mut Vec<Item>,
    ) {
        let Some((origin, target, external)) =
            self.resolve(item.origin, item.external.as_ref(), real)
        else {
            // A dangling or non-markdown link is not worth a notice.
            let disk = self.disk_path(item.origin, item.external.as_ref(), real);
            if is_markdown(display.file_name().unwrap_or_default())
                || std::fs::metadata(disk).is_ok_and(|m| m.is_dir())
            {
                self.note(format!(
                    "rule symlink {} skipped: {}",
                    display.display(),
                    self.refusal()
                ));
            }
            return;
        };
        let disk = self.disk_path(origin, external.as_ref(), &target);
        let Ok(metadata) = std::fs::metadata(&disk) else {
            return;
        };
        if metadata.is_dir() {
            if depth + 1 >= self.limits.depth {
                self.note(format!(
                    "rule directory {} not scanned: deeper than {} levels (limit)",
                    display.display(),
                    self.limits.depth
                ));
            } else if self
                .visited
                .insert(self.key(origin, external.as_ref(), &target))
            {
                children.push(Item {
                    origin,
                    real: target,
                    display,
                    depth: depth + 1,
                    external,
                });
            }
        } else if metadata.is_file() && is_markdown(display.file_name().unwrap_or_default()) {
            if target.extension().is_none_or(|extension| extension != "md") {
                self.note(format!(
                    "rule symlink {} skipped: its target is not a .md file",
                    display.display()
                ));
                return;
            }
            self.files.push(RuleFile {
                display,
                origin,
                real: target,
                external,
            });
        }
    }

    fn disk_path(
        &self,
        origin: RuleOrigin,
        external: Option<&Arc<WorkspaceBinding>>,
        real: &Path,
    ) -> PathBuf {
        match origin {
            RuleOrigin::Workspace => self.workspace.root().join(real),
            RuleOrigin::User => self
                .user
                .map(|user| user.binding.root().join(real))
                .unwrap_or_default(),
            RuleOrigin::External => external
                .map(|binding| binding.root().join(real))
                .unwrap_or_default(),
        }
    }

    /// The canonical location of the link at `real` under `origin`, when it
    /// is inside the workspace (not `.git`), the user's `~/.claude/rules`, or
    /// the opted-in external root (not `.git`). External targets are opened
    /// through a capability on their canonical parent directory.
    fn resolve(
        &self,
        origin: RuleOrigin,
        external: Option<&Arc<WorkspaceBinding>>,
        real: &Path,
    ) -> Option<(RuleOrigin, PathBuf, Option<Arc<WorkspaceBinding>>)> {
        let canonical = std::fs::canonicalize(self.disk_path(origin, external, real)).ok()?;
        let hidden_git = |path: &Path| path.components().any(|part| part.as_os_str() == ".git");
        if let Ok(inside) = canonical.strip_prefix(self.workspace.root())
            && origin == RuleOrigin::Workspace
            && !hidden_git(inside)
            && !inside.as_os_str().is_empty()
        {
            return Some((RuleOrigin::Workspace, inside.to_path_buf(), None));
        }
        if let Some(user) = self.user
            && let Ok(inside) = canonical.strip_prefix(user.binding.root())
            && inside.starts_with("rules")
        {
            return Some((RuleOrigin::User, inside.to_path_buf(), None));
        }
        let root = self.external_root?;
        if hidden_git(&canonical) {
            return None;
        }
        let root = std::fs::canonicalize(root).ok()?;
        let inside = canonical.strip_prefix(&root).ok()?;
        if inside.as_os_str().is_empty() || hidden_git(inside) {
            return None;
        }
        let parent = WorkspaceBinding::capture(canonical.parent()?).ok()?;
        let name = PathBuf::from(canonical.file_name()?);
        Some((RuleOrigin::External, name, Some(Arc::new(parent))))
    }

    fn finish(mut self, origin: RuleOrigin, base: &Path) -> RuleScan {
        let mut seen = HashSet::new();
        // The same physical file reached through two links loads once.
        self.files.retain(|file| seen.insert(file.identity()));
        self.files.sort_by(|a, b| a.display.cmp(&b.display));
        if self.files.len() > self.limits.files {
            let skipped: Vec<String> = self
                .files
                .split_off(self.limits.files)
                .iter()
                .map(|file| file.display.display().to_string())
                .collect();
            let shown = skipped
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            let more = skipped.len().saturating_sub(5);
            self.note(format!(
                "{} rule files under {} not loaded: more than {} files (limit); skipped {}{}",
                skipped.len(),
                base.display(),
                self.limits.files,
                shown,
                if more > 0 {
                    format!(" and {more} more")
                } else {
                    String::new()
                }
            ));
        }
        if self.dropped_notes > 0 {
            self.notes
                .push(format!("... and {} more rule notices", self.dropped_notes));
        }
        let _ = origin;
        RuleScan {
            files: self.files,
            notes: self.notes,
        }
    }
}

fn is_markdown(name: &std::ffi::OsStr) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|extension| extension == "md")
}
