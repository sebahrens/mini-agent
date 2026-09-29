//! Workspace Git metadata that sandboxed processes may read but never rewrite.
//!
//! The workspace sandbox profiles grant write access to the whole workspace,
//! which includes its `.git` directory. Ordinary Git work inside the sandbox
//! (`git add`, `git commit`, refs, the index, objects and logs) needs that, but
//! a few metadata entries decide which code Git runs or how it transforms
//! content: `config` (for example `core.fsmonitor`, filter and diff drivers),
//! `hooks`, `info` (attributes that select those drivers) and `modules`
//! (nested submodule repositories with their own config and hooks). A
//! prompt-injected command that rewrites them would plant code that later runs
//! outside the sandbox. This module finds those entries so every workspace
//! profile can keep them read-only:
//!
//! * Seatbelt denies `file-write*` on each entry by path, in both its
//!   configured and its resolved spelling, whether or not it exists yet.
//! * bubblewrap cannot bind a path that does not exist, and a bind source
//!   named by a model-writable path could be swapped for a symlink to a host
//!   file. Each existing entry inside the workspace is therefore opened here
//!   without following symlinks and handed to bubblewrap as a descriptor
//!   (`--ro-bind-fd`), which binds exactly that inode and closes the
//!   descriptor before the sandboxed program starts. The `.git` directory
//!   itself is additionally bound onto itself (`--bind-fd`, still writable) so
//!   it is a mount point that cannot be renamed away and replaced.

#[cfg(unix)]
use std::path::Component;
use std::path::{Path, PathBuf};

/// Entries of a Git common directory that configure code execution or
/// content transforms.
const COMMON_DIR_ENTRIES: [&str; 4] = ["config", "hooks", "info", "modules"];
/// Entries of a Git directory that select configuration: per-worktree
/// configuration and the pointer Git follows to the common directory (a
/// planted `commondir` would redirect Git to a model-written `config`).
const GIT_DIR_ENTRIES: [&str; 2] = ["config.worktree", "commondir"];
/// Upper bound on bytes read from a `.git` gitfile or a `commondir` pointer.
const POINTER_FILE_LIMIT: u64 = 4096;

/// The Git metadata layout of one workspace, in configured spellings.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct GitMetadataLayout {
    /// The `<workspace>/.git` directory (or the directory a `.git` symlink
    /// names), pinned so it cannot be renamed away and back.
    pub(super) pinned_dir: Option<PathBuf>,
    /// Paths that must stay read-only; they may not exist.
    pub(super) read_only: Vec<PathBuf>,
}

/// Discover the Git metadata of a repository whose top level is `workspace`.
///
/// Only `<workspace>/.git` is consulted: a repository above the workspace is
/// outside every writable root already. A linked worktree's gitfile is
/// followed to its Git directory and that directory's `commondir`.
pub(super) fn discover(workspace: &Path) -> GitMetadataLayout {
    let dot_git = workspace.join(".git");
    let Ok(metadata) = std::fs::symlink_metadata(&dot_git) else {
        return GitMetadataLayout::default();
    };
    let mut layout = GitMetadataLayout::default();
    let git_dir = if metadata.is_dir() {
        layout.pinned_dir = Some(dot_git.clone());
        dot_git
    } else if metadata.is_file() {
        // The gitfile decides which Git directory the host resolves.
        layout.read_only.push(dot_git.clone());
        match read_pointer(&dot_git, Some("gitdir:")) {
            Some(target) => workspace.join(target),
            None => return layout,
        }
    } else if metadata.file_type().is_symlink() {
        // Git follows the link; protect the directory it currently names.
        layout.pinned_dir = Some(dot_git.clone());
        dot_git
    } else {
        return GitMetadataLayout::default();
    };
    let common_dir = read_pointer(&git_dir.join("commondir"), None)
        .map(|target| git_dir.join(target))
        .unwrap_or_else(|| git_dir.clone());
    for entry in COMMON_DIR_ENTRIES {
        push_unique(&mut layout.read_only, common_dir.join(entry));
    }
    for entry in GIT_DIR_ENTRIES {
        push_unique(&mut layout.read_only, git_dir.join(entry));
    }
    layout
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

/// Read a one-line pointer file (`gitdir: <path>` or a bare `commondir`).
fn read_pointer(path: &Path, prefix: Option<&str>) -> Option<PathBuf> {
    use std::io::Read;

    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    // A FIFO or symlink swapped in after the type check must not block the
    // launch or redirect the read.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(
        &mut options,
        libc::O_NONBLOCK | libc::O_NOFOLLOW,
    );
    let mut contents = String::new();
    options
        .open(path)
        .ok()?
        .take(POINTER_FILE_LIMIT)
        .read_to_string(&mut contents)
        .ok()?;
    let line = contents.lines().next()?;
    let value = match prefix {
        Some(prefix) => line.strip_prefix(prefix)?.trim(),
        None => line.trim(),
    };
    (!value.is_empty()).then(|| PathBuf::from(value))
}

/// Every spelling Seatbelt must deny for `path`: the configured one and the
/// resolved one (Seatbelt matches the kernel's resolved vnode path).
fn spellings(path: &Path) -> Vec<PathBuf> {
    let mut spellings = vec![path.to_path_buf()];
    if let Some(resolved) = super::seatbelt_resolved_spelling(path) {
        push_unique(&mut spellings, resolved);
    }
    spellings
}

/// The body of a Seatbelt `(deny file-write* ...)` rule for the workspace's
/// Git metadata, or an empty string when the workspace has no `.git`.
///
/// A spelling Seatbelt cannot represent is skipped rather than failing the
/// whole profile: it names a path the profile could not express anyway.
///
/// The `.git` directory itself is denied as a `literal` (its entries stay
/// writable) so it cannot be renamed away, edited under another name and
/// renamed back.
pub(super) fn seatbelt_write_denies(workspace: &Path) -> String {
    let layout = discover(workspace);
    let mut filters: Vec<String> = Vec::new();
    let planned = layout
        .pinned_dir
        .iter()
        .map(|path| (path, "literal"))
        .chain(layout.read_only.iter().map(|path| (path, "subpath")));
    for (path, filter) in planned {
        for spelling in spellings(path) {
            if spelling.parent().is_none() {
                continue;
            }
            if let Ok(literal) = super::seatbelt_string_literal(&spelling, "Git metadata path") {
                let rule = format!("\n    ({filter} \"{literal}\")");
                if !filters.contains(&rule) {
                    filters.push(rule);
                }
            }
        }
    }
    filters.concat()
}

/// Descriptors for the bubblewrap arms, bound after the writable workspace.
#[derive(Debug, Default)]
pub(super) struct BwrapGitMetadataBinds {
    #[cfg(unix)]
    entries: Vec<BwrapGitBind>,
}

#[cfg(unix)]
#[derive(Debug)]
struct BwrapGitBind {
    /// Path relative to the workspace root; never empty, no `..`.
    relative: PathBuf,
    read_only: bool,
    fd: std::os::fd::OwnedFd,
}

/// Lowest descriptor number the bind descriptors are moved to, far above the
/// fixed numbers other launch steps install (stdio, the snapshot's fd 3 and
/// the workspace authority's fd 197).
#[cfg(unix)]
const BIND_FD_FLOOR: i32 = 256;

impl BwrapGitMetadataBinds {
    /// No Git metadata binds (no repository, or a bubblewrap without
    /// `--ro-bind-fd`).
    pub(super) fn none() -> Self {
        Self::default()
    }

    /// Open the workspace's existing Git metadata entries beneath the
    /// canonical `workspace_root`. Entries outside the workspace are not
    /// visible in the sandbox and are skipped, as is anything that is missing,
    /// is not a regular file or directory, or is reached through a symlink.
    #[cfg(unix)]
    pub(super) fn open(workspace_root: &Path) -> Self {
        let Ok(root) = std::fs::canonicalize(workspace_root) else {
            return Self::none();
        };
        let layout = discover(&root);
        let Some(root_fd) = open_directory(&root) else {
            return Self::none();
        };
        let mut entries: Vec<BwrapGitBind> = Vec::new();
        let planned = layout
            .pinned_dir
            .iter()
            .map(|path| (path, false))
            .chain(layout.read_only.iter().map(|path| (path, true)));
        for (path, read_only) in planned {
            let Ok(canonical) = std::fs::canonicalize(path) else {
                continue;
            };
            let Some(relative) = relative_beneath(&root, &canonical) else {
                continue;
            };
            if entries.iter().any(|entry| entry.relative == relative) {
                continue;
            }
            let Some(fd) = open_beneath(&root_fd, &relative, !read_only) else {
                continue;
            };
            entries.push(BwrapGitBind {
                relative,
                read_only,
                fd,
            });
        }
        Self { entries }
    }

    #[cfg(not(unix))]
    pub(super) fn open(_workspace_root: &Path) -> Self {
        Self::none()
    }

    /// Append the bind options (after the writable workspace bind) and keep
    /// the descriptors open across `exec` of bubblewrap only.
    #[cfg(unix)]
    #[allow(unsafe_code)]
    pub(super) fn apply(self, command: &mut super::Command, sandbox_root: &Path) {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;

        if self.entries.is_empty() {
            return;
        }
        for entry in &self.entries {
            let option = if entry.read_only {
                "--ro-bind-fd"
            } else {
                "--bind-fd"
            };
            command
                .arg(option)
                .arg(entry.fd.as_raw_fd().to_string())
                .arg(sandbox_root.join(&entry.relative));
        }
        let entries = self.entries;
        // SAFETY: the closure only calls the async-signal-safe `fcntl` on
        // descriptors it owns and performs no allocation.
        unsafe {
            command.as_std_mut().pre_exec(move || {
                for entry in &entries {
                    let fd = entry.fd.as_raw_fd();
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags == -1
                        || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }

    #[cfg(not(unix))]
    pub(super) fn apply(self, _command: &mut super::Command, _sandbox_root: &Path) {}
}

/// `path` relative to `root` when it lies strictly beneath it.
#[cfg(unix)]
fn relative_beneath(root: &Path, path: &Path) -> Option<PathBuf> {
    let relative = path.strip_prefix(root).ok()?;
    let normal = relative.components().next().is_some()
        && relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    normal.then(|| relative.to_path_buf())
}

#[cfg(unix)]
fn path_flags() -> libc::c_int {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        libc::O_PATH
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        // Without O_PATH, a read-only non-blocking open never waits on a FIFO;
        // the type check below rejects it anyway.
        libc::O_RDONLY | libc::O_NONBLOCK
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn open_directory(path: &Path) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `path` is a valid NUL-terminated string.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            path_flags() | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    // SAFETY: a non-negative `fd` was just returned to us and is owned here.
    (fd >= 0).then(|| unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

/// Open `relative` beneath `root` one component at a time without following
/// symlinks, then move the descriptor above [`BIND_FD_FLOOR`].
#[cfg(unix)]
#[allow(unsafe_code)]
fn open_beneath(
    root: &std::os::fd::OwnedFd,
    relative: &Path,
    require_directory: bool,
) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    let components: Vec<_> = relative.components().collect();
    let mut current: Option<OwnedFd> = None;
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return None;
        };
        let name = std::ffi::CString::new(name.as_bytes()).ok()?;
        let last = index + 1 == components.len();
        let mut flags = path_flags() | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        if !last || require_directory {
            flags |= libc::O_DIRECTORY;
        }
        let parent = current.as_ref().unwrap_or(root).as_raw_fd();
        // SAFETY: `parent` is an open directory descriptor and `name` is a
        // valid NUL-terminated component.
        let fd = unsafe { libc::openat(parent, name.as_ptr(), flags) };
        if fd < 0 {
            return None;
        }
        // SAFETY: a non-negative `fd` was just returned to us and is owned here.
        current = Some(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    let opened = current?;
    // SAFETY: `stat` is plain data and `fstat` fills it for an open descriptor.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(opened.as_raw_fd(), &mut stat) } != 0 {
        return None;
    }
    let kind = stat.st_mode & libc::S_IFMT;
    let acceptable = if require_directory {
        kind == libc::S_IFDIR
    } else {
        kind == libc::S_IFDIR || kind == libc::S_IFREG
    };
    if !acceptable {
        return None;
    }
    // SAFETY: duplicating an owned descriptor; the duplicate is owned below.
    let moved = unsafe { libc::fcntl(opened.as_raw_fd(), libc::F_DUPFD_CLOEXEC, BIND_FD_FLOOR) };
    // SAFETY: a non-negative `moved` was just returned to us and is owned here.
    (moved >= 0).then(|| unsafe { OwnedFd::from_raw_fd(moved) })
}

#[cfg(all(test, unix))]
impl BwrapGitMetadataBinds {
    /// `(read_only, relative)` of every planned bind, in argv order.
    pub(super) fn planned(&self) -> Vec<(bool, PathBuf)> {
        self.entries
            .iter()
            .map(|entry| (entry.read_only, entry.relative.clone()))
            .collect()
    }
}

/// Hand-built repository layouts; no `git` executable is needed.
#[cfg(all(test, unix))]
pub(super) mod fixtures {
    use std::path::{Path, PathBuf};

    /// `<root>/<name>` as a repository with `config`, `hooks` and `info`
    /// present and `modules` and `config.worktree` absent.
    pub(in crate::sandbox) fn plain_repository(root: &Path, name: &str) -> PathBuf {
        let workspace = root.join(name);
        let git_dir = workspace.join(".git");
        for directory in ["hooks", "info", "objects", "refs/heads"] {
            std::fs::create_dir_all(git_dir.join(directory)).unwrap();
        }
        std::fs::write(git_dir.join("config"), "[core]\n\tbare = false\n").unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        workspace
    }

    /// A linked worktree `<root>/wt` of `<root>/main`, whose gitfile names
    /// the main repository through `gitdir_spelling_root` (a directory that
    /// resolves to `root`).
    pub(in crate::sandbox) fn linked_worktree(
        root: &Path,
        gitdir_spelling_root: &Path,
    ) -> (PathBuf, PathBuf) {
        let main = plain_repository(root, "main");
        let worktree_git_dir = main.join(".git/worktrees/wt");
        std::fs::create_dir_all(&worktree_git_dir).unwrap();
        std::fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();
        std::fs::write(worktree_git_dir.join("HEAD"), "ref: refs/heads/wt\n").unwrap();
        std::fs::write(worktree_git_dir.join("config.worktree"), "").unwrap();
        let worktree = root.join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!(
                "gitdir: {}\n",
                gitdir_spelling_root
                    .join("main/.git/worktrees/wt")
                    .display()
            ),
        )
        .unwrap();
        (main, worktree)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        /// A canonical scratch root, so relative expectations are exact.
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("mini-agent-git-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(std::fs::canonicalize(path).unwrap())
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn discovery_of_a_plain_repository_pins_dot_git_and_protects_common_entries() {
        let scratch = Scratch::new();
        let workspace = fixtures::plain_repository(&scratch.0, "repo");
        let git_dir = workspace.join(".git");

        let layout = discover(&workspace);

        assert_eq!(layout.pinned_dir.as_deref(), Some(git_dir.as_path()));
        assert_eq!(
            layout.read_only,
            vec![
                git_dir.join("config"),
                git_dir.join("hooks"),
                git_dir.join("info"),
                git_dir.join("modules"),
                git_dir.join("config.worktree"),
                git_dir.join("commondir"),
            ]
        );
    }

    #[test]
    fn discovery_of_a_linked_worktree_follows_the_gitfile_and_commondir() {
        let scratch = Scratch::new();
        let (main, worktree) = fixtures::linked_worktree(&scratch.0, &scratch.0);

        let layout = discover(&worktree);

        assert_eq!(layout.pinned_dir, None, "a gitfile is not a directory");
        let worktree_git_dir = main.join(".git/worktrees/wt");
        let common_dir = worktree_git_dir.join("../..");
        assert_eq!(
            layout.read_only,
            vec![
                worktree.join(".git"),
                common_dir.join("config"),
                common_dir.join("hooks"),
                common_dir.join("info"),
                common_dir.join("modules"),
                worktree_git_dir.join("config.worktree"),
                worktree_git_dir.join("commondir"),
            ]
        );
    }

    #[test]
    fn discovery_without_a_repository_protects_nothing() {
        let scratch = Scratch::new();
        assert_eq!(discover(&scratch.0), GitMetadataLayout::default());
        assert_eq!(seatbelt_write_denies(&scratch.0), "");
        assert!(BwrapGitMetadataBinds::open(&scratch.0).planned().is_empty());
    }

    #[test]
    fn seatbelt_denies_cover_configured_and_resolved_spellings() {
        let scratch = Scratch::new();
        let alias = scratch.0.join("alias");
        let real = scratch.0.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let (main, worktree) = fixtures::linked_worktree(&real, &alias);

        let denies = seatbelt_write_denies(&worktree);

        let subpath = |path: &Path| format!("(subpath \"{}\")", path.display());
        // The gitfile names the repository through the symlinked alias ...
        let configured = alias.join("main/.git/worktrees/wt/../..");
        for entry in ["config", "hooks", "info", "modules"] {
            assert!(
                denies.contains(&subpath(&configured.join(entry))),
                "{denies}"
            );
            // ... and Seatbelt matches the resolved path, which is denied even
            // for an entry that does not exist yet (`modules`).
            assert!(
                denies.contains(&subpath(&main.join(".git").join(entry))),
                "{denies}"
            );
        }
        assert!(denies.contains(&subpath(&worktree.join(".git"))));
        assert!(denies.contains(&subpath(&main.join(".git/worktrees/wt/config.worktree"))));
        assert!(!denies.contains(&subpath(&main.join(".git"))), "{denies}");
    }

    #[test]
    fn bwrap_binds_pin_dot_git_and_skip_missing_or_symlinked_entries() {
        let scratch = Scratch::new();
        let workspace = fixtures::plain_repository(&scratch.0, "repo");
        // A model-planted symlink must never become a bind source.
        let outside = scratch.0.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::remove_dir_all(workspace.join(".git/info")).unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join(".git/info")).unwrap();

        let planned = BwrapGitMetadataBinds::open(&workspace).planned();

        assert_eq!(
            planned,
            vec![
                (false, PathBuf::from(".git")),
                (true, PathBuf::from(".git/config")),
                (true, PathBuf::from(".git/hooks")),
            ]
        );
    }

    #[test]
    fn bwrap_binds_of_a_linked_worktree_cover_only_its_gitfile() {
        let scratch = Scratch::new();
        let (_main, worktree) = fixtures::linked_worktree(&scratch.0, &scratch.0);

        // The common directory is outside the workspace, so it is not visible
        // inside the sandbox at all.
        let planned = BwrapGitMetadataBinds::open(&worktree).planned();

        assert_eq!(planned, vec![(true, PathBuf::from(".git"))]);
    }
}
