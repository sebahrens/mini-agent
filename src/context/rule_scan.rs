//! Deterministic, reporting discovery of `.claude/rules/**/*.md` files.
//!
//! The walk visits directories and entries in sorted name order, so which
//! rules survive a cap never depends on `read_dir` order. Anything the walk
//! cannot honour (file-count, depth or entry caps, links it refuses) is
//! returned as a note naming the item and the limit, so the caller can show it
//! instead of silently dropping the rule.
//!
//! Symlinked rule files and directories are followed only when explicitly
//! allowed (`user_instruction_files`) and only when the canonical target is
//! inside the workspace or inside the user's `~/.claude/rules`; the target is
//! then opened through a capability on its real location, never through the
//! link.
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
}

/// One discovered rule file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RuleFile {
    /// The spelled path used for display and labels
    /// (`.claude/rules/a.md`, `rules/a.md` for user rules).
    pub(crate) display: PathBuf,
    pub(crate) origin: RuleOrigin,
    /// The link-free path under the origin's binding the file is read from.
    pub(crate) real: PathBuf,
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
    follow_links: bool,
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
}

/// Scans `rules_dir` (workspace relative, e.g. `.claude/rules`) and, when
/// `user` is given, the user's `rules` directory with the same limits.
pub(crate) fn scan(
    workspace: &WorkspaceBinding,
    rules_dir: &Path,
    user: Option<&UserClaudeDir>,
    follow_links: bool,
    limits: Limits,
) -> (RuleScan, RuleScan) {
    let project = scan_unit(
        workspace,
        user,
        follow_links,
        limits,
        RuleOrigin::Workspace,
        rules_dir,
    );
    let user_scan = match user {
        Some(user) => scan_unit(
            workspace,
            Some(user),
            follow_links,
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
    follow_links: bool,
    limits: Limits,
    origin: RuleOrigin,
    base: &Path,
) -> RuleScan {
    let mut walk = Walk {
        workspace,
        user,
        follow_links,
        limits,
        scanned: 0,
        visited: HashSet::new(),
        files: Vec::new(),
        notes: Vec::new(),
        dropped_notes: 0,
    };
    walk.visited.insert((origin, base.to_path_buf()));
    let mut pending = vec![Item {
        origin,
        real: base.to_path_buf(),
        display: base.to_path_buf(),
        depth: 0,
    }];
    while let Some(item) = pending.pop() {
        walk.visit(item, &mut pending);
    }
    walk.finish(origin, base)
}

impl Walk<'_> {
    fn binding(&self, origin: RuleOrigin) -> Option<&WorkspaceBinding> {
        match origin {
            RuleOrigin::Workspace => Some(self.workspace),
            RuleOrigin::User => self.user.map(|user| &*user.binding),
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
        let Some(binding) = self.binding(item.origin) else {
            return;
        };
        let Ok(directory) = binding.open_dir_relative(&item.real) else {
            return;
        };
        let Ok(entries) = directory.read_dir(".") else {
            return;
        };
        let mut found = Vec::new();
        let mut capped = false;
        for entry in entries {
            self.scanned += 1;
            if self.scanned > self.limits.entries {
                capped = true;
                break;
            }
            let Ok(entry) = entry else { continue };
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            found.push((entry.file_name(), kind));
        }
        found.sort_by(|a, b| a.0.cmp(&b.0));
        if capped {
            self.note(format!(
                "rule scan stopped after {} directory entries (limit) while reading {}; \
                 rules not yet reached were not loaded and the result may depend on directory \
                 order: split or prune the directory",
                self.limits.entries,
                item.display.display()
            ));
        }
        let mut children = Vec::new();
        for (name, kind) in found {
            let display = item.display.join(&name);
            let real = item.real.join(&name);
            if kind.is_dir() {
                if item.depth + 1 < self.limits.depth {
                    if self.visited.insert((item.origin, real.clone())) {
                        children.push(Item {
                            origin: item.origin,
                            real,
                            display,
                            depth: item.depth + 1,
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
        if !self.follow_links {
            // A dangling or non-markdown link is not worth a notice.
            if is_markdown(display.file_name().unwrap_or_default())
                || std::fs::metadata(self.disk_path(item.origin, real)).is_ok_and(|m| m.is_dir())
            {
                self.note(format!(
                    "rule symlink {} skipped: following rule symlinks requires \
                     user_instruction_files = true",
                    display.display()
                ));
            }
            return;
        }
        let Some((origin, target)) = self.resolve(item.origin, real) else {
            self.note(format!(
                "rule symlink {} skipped: its target is outside the workspace and ~/.claude/rules \
                 (or unreadable)",
                display.display()
            ));
            return;
        };
        let disk = self.disk_path(origin, &target);
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
            } else if self.visited.insert((origin, target.clone())) {
                children.push(Item {
                    origin,
                    real: target,
                    display,
                    depth: depth + 1,
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
            });
        }
    }

    fn disk_path(&self, origin: RuleOrigin, real: &Path) -> PathBuf {
        match origin {
            RuleOrigin::Workspace => self.workspace.root().join(real),
            RuleOrigin::User => self
                .user
                .map(|user| user.binding.root().join(real))
                .unwrap_or_default(),
        }
    }

    /// The canonical location of the link at `real` under `origin`, when it
    /// is inside the workspace (not `.git`) or the user's `~/.claude/rules`.
    fn resolve(&self, origin: RuleOrigin, real: &Path) -> Option<(RuleOrigin, PathBuf)> {
        let canonical = std::fs::canonicalize(self.disk_path(origin, real)).ok()?;
        if let Ok(inside) = canonical.strip_prefix(self.workspace.root()) {
            let hidden_git = inside.components().any(|part| part.as_os_str() == ".git");
            if origin == RuleOrigin::Workspace && !hidden_git && !inside.as_os_str().is_empty() {
                return Some((RuleOrigin::Workspace, inside.to_path_buf()));
            }
        }
        let user = self.user?;
        let inside = canonical.strip_prefix(user.binding.root()).ok()?;
        inside
            .starts_with("rules")
            .then(|| (RuleOrigin::User, inside.to_path_buf()))
    }

    fn finish(mut self, origin: RuleOrigin, base: &Path) -> RuleScan {
        let mut seen = HashSet::new();
        // The same physical file reached through two links loads once.
        self.files
            .retain(|file| seen.insert((file.origin, file.real.clone())));
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
