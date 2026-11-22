//! Default read/execute roots for toolchains the host `PATH` resolves, for the Windows
//! AppContainer shell.
//!
//! The AppContainer profile can read only the workspace, the application cache, the selected
//! shell and objects that already admit `ALL APPLICATION PACKAGES` (the Windows directory and
//! `Program Files`). Interpreters installed anywhere else (a runner tool cache, `C:\Python312`,
//! `%LOCALAPPDATA%\Programs\Python`, a Chocolatey or Scoop directory) are on `PATH` but fail with
//! "not recognized" or "Access is denied" inside the sandbox. This module mirrors what Linux does
//! with `sandbox-toolchain-roots`: it resolves a fixed list of well-known command names on the
//! host `PATH` and proposes the directory that holds each resolved executable as a read/execute
//! root.
//!
//! The result is a *proposal* that the launcher still validates and grants best effort:
//!
//! * only read and execute is ever granted, never write, and never to a root that overlaps the
//!   workspace, the application cache, the private control root or a configured writable root;
//! * `PATH` entries under the Windows directory or `Program Files` are skipped (they are already
//!   readable, and their DACLs are not ours to rewrite);
//! * the drive root, the user profile and its ancestors, directories that directly hold
//!   credential folders (`.ssh`, `.aws`, ...), and trees larger than
//!   [`MAX_DISCOVERED_ROOT_ENTRIES`] (or that overrun [`MAX_DISCOVERED_TOTAL_ENTRIES`] in all) are skipped, so an unusual `PATH` entry cannot expose a home
//!   directory or make every launch walk an enormous tree;
//! * at most [`MAX_DISCOVERED_ROOTS`] roots are proposed.
//!
//! `windows-appcontainer-toolchain-discovery = false` turns the proposal off, and
//! `windows-appcontainer-read-roots` still adds explicit roots.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Commands whose install directory is proposed when the host `PATH` resolves them.
pub(crate) const TOOLCHAIN_COMMANDS: &[&str] = &[
    "python", "python3", "pip", "pip3", "node", "npm", "npx", "pnpm", "yarn", "deno", "bun", "uv",
    "cargo", "rustc", "go", "java", "javac", "mvn", "gradle", "dotnet", "ruby", "gem", "perl",
    "php", "cmake", "ninja", "make",
];

/// Upper bound on proposed roots.
pub(crate) const MAX_DISCOVERED_ROOTS: usize = 8;

/// A proposed tree larger than this is skipped: every launch grants and revokes an ACE
/// on every object below the root.
pub(crate) const MAX_DISCOVERED_ROOT_ENTRIES: usize = 10_000;

/// Upper bound on the objects below all proposed roots together; a proposal that would exceed it
/// is skipped (the earlier entries of [`TOOLCHAIN_COMMANDS`] win). Every shell launch writes and
/// later revokes one ACE per object, so this bounds the per-command latency the default adds.
pub(crate) const MAX_DISCOVERED_TOTAL_ENTRIES: usize = 12_000;

/// Directory names that mark a credential store; a proposed root may not directly contain one.
const CREDENTIAL_DIRECTORIES: &[&str] = &[
    ".ssh",
    ".aws",
    ".azure",
    ".gnupg",
    ".kube",
    ".docker",
    ".config",
    ".gcloud",
    ".npmrc",
    ".netrc",
    ".git-credentials",
];

/// Directory names that are never proposed as a whole.
const BROAD_DIRECTORY_NAMES: &[&str] = &[
    "appdata",
    "windowsapps",
    "local",
    "locallow",
    "roaming",
    "documents",
    "desktop",
    "downloads",
    "temp",
    "tmp",
    "users",
    "windows",
    "system32",
];

/// Host facts the proposal depends on, injected so tests need no real Windows host.
pub(crate) struct DiscoveryEnvironment {
    /// The host `PATH` value.
    pub(crate) path: std::ffi::OsString,
    /// Executable suffixes (`.EXE`, `.CMD`, ...); empty on hosts that do not use them.
    pub(crate) extensions: Vec<String>,
    /// The user profile directory.
    pub(crate) home: Option<PathBuf>,
    /// Trees already readable to every application package (Windows, Program Files).
    pub(crate) protected_roots: Vec<PathBuf>,
}

impl DiscoveryEnvironment {
    /// The facts of the running host.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn from_process() -> Option<Self> {
        let path = std::env::var_os("PATH")?;
        let extensions = if cfg!(windows) {
            std::env::var("PATHEXT")
                .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
                .split(';')
                .filter(|extension| extension.starts_with('.'))
                .map(str::to_string)
                .collect()
        } else {
            Vec::new()
        };
        let protected_roots = [
            "SystemRoot",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramW6432",
        ]
        .iter()
        .filter_map(|name| std::env::var_os(name))
        .map(PathBuf::from)
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .collect();
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .and_then(|path| std::fs::canonicalize(path).ok());
        Some(Self {
            path,
            extensions,
            home,
            protected_roots,
        })
    }
}

/// First match of `name` in the directories of `path`, trying each suffix.
pub(crate) fn resolve_on_path(name: &str, path: &OsStr, extensions: &[String]) -> Option<PathBuf> {
    for directory in std::env::split_paths(path) {
        if directory.as_os_str().is_empty() || !directory.is_absolute() {
            continue;
        }
        let bare = directory.join(name);
        let mut candidates = vec![bare.clone()];
        for extension in extensions {
            let mut spelled = bare.clone().into_os_string();
            spelled.push(extension);
            candidates.push(PathBuf::from(spelled));
        }
        if let Some(found) = candidates.into_iter().find(|candidate| candidate.is_file()) {
            return Some(found);
        }
    }
    None
}

fn same_or_inside(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

fn lowered_name(path: &Path) -> Option<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
}

/// The directory to propose for a resolved executable: its own directory, except that a
/// `Scripts` directory (pip, pytest, ...) or a `bin` directory of an install prefix (Go, Ruby,
/// a JDK, MinGW) proposes the prefix above it, where the standard library lives. A `bin` under a
/// dot-directory (`.cargo\bin`, `.local\bin`) stays as it is: dot-directories also hold
/// configuration and credentials.
fn proposed_directory(executable: &Path) -> Option<PathBuf> {
    let directory = executable.parent()?;
    let prefix = directory.parent().map(Path::to_path_buf);
    match lowered_name(directory).as_deref() {
        Some("scripts") => prefix,
        Some("bin")
            if prefix
                .as_deref()
                .and_then(lowered_name)
                .is_some_and(|name| !name.starts_with('.')) =>
        {
            prefix
        }
        _ => Some(directory.to_path_buf()),
    }
}

/// Why a directory cannot be proposed, for debug logs and tests.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Rejection {
    NotADirectory,
    DriveRoot,
    AlreadyReadable,
    BroadDirectory,
    ContainsHomeOrAncestor,
    ContainsCredentials,
    TooLarge,
    OverBudget,
}

fn credential_child_present(directory: &Path) -> bool {
    CREDENTIAL_DIRECTORIES
        .iter()
        .any(|name| std::fs::symlink_metadata(directory.join(name)).is_ok())
}

/// Count the entries below `root` without following links, stopping at `limit + 1`.
pub(crate) fn bounded_entry_count(root: &Path, limit: usize) -> usize {
    let mut count = 0usize;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            count += 1;
            if count > limit {
                return count;
            }
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                pending.push(entry.path());
            }
        }
    }
    count
}

/// The entry count of an acceptable proposal.
fn judge(
    directory: &Path,
    environment: &DiscoveryEnvironment,
    entry_limit: usize,
) -> Result<usize, Rejection> {
    if !std::fs::symlink_metadata(directory).is_ok_and(|meta| meta.is_dir()) {
        return Err(Rejection::NotADirectory);
    }
    if directory.parent().is_none() {
        return Err(Rejection::DriveRoot);
    }
    if environment
        .protected_roots
        .iter()
        .any(|protected| same_or_inside(directory, protected))
    {
        return Err(Rejection::AlreadyReadable);
    }
    if lowered_name(directory).is_some_and(|name| BROAD_DIRECTORY_NAMES.contains(&name.as_str())) {
        return Err(Rejection::BroadDirectory);
    }
    if let Some(home) = &environment.home
        && same_or_inside(home, directory)
    {
        return Err(Rejection::ContainsHomeOrAncestor);
    }
    if credential_child_present(directory) {
        return Err(Rejection::ContainsCredentials);
    }
    let entries = bounded_entry_count(directory, entry_limit);
    if entries > entry_limit {
        return Err(Rejection::TooLarge);
    }
    Ok(entries)
}

/// The read/execute roots to propose, deduplicated and without nested entries. Each root is a
/// canonical directory; nothing here touches an ACL.
pub(crate) fn discover(environment: &DiscoveryEnvironment) -> Vec<PathBuf> {
    discover_within(
        environment,
        MAX_DISCOVERED_ROOT_ENTRIES,
        MAX_DISCOVERED_TOTAL_ENTRIES,
    )
}

fn discover_within(
    environment: &DiscoveryEnvironment,
    root_limit: usize,
    total_limit: usize,
) -> Vec<PathBuf> {
    let mut proposed: Vec<(PathBuf, usize)> = Vec::new();
    for name in TOOLCHAIN_COMMANDS {
        let Some(executable) = resolve_on_path(name, &environment.path, &environment.extensions)
        else {
            continue;
        };
        let Some(directory) = proposed_directory(&executable) else {
            continue;
        };
        let Ok(directory) = std::fs::canonicalize(&directory) else {
            continue;
        };
        if proposed
            .iter()
            .any(|(known, _)| same_or_inside(&directory, known))
        {
            continue;
        }
        let verdict = judge(&directory, environment, root_limit).and_then(|entries| {
            // A broader accepted root replaces the narrower ones it contains.
            let replaced: usize = proposed
                .iter()
                .filter(|(known, _)| same_or_inside(known, &directory))
                .map(|(_, count)| count)
                .sum();
            let total: usize = proposed.iter().map(|(_, count)| count).sum();
            if total - replaced + entries > total_limit {
                Err(Rejection::OverBudget)
            } else {
                Ok(entries)
            }
        });
        match verdict {
            Ok(entries) => {
                proposed.retain(|(known, _)| !same_or_inside(known, &directory));
                proposed.push((directory, entries));
            }
            Err(reason) => {
                tracing::debug!(
                    command = *name,
                    reason = ?reason,
                    "sandbox: skipped a host PATH toolchain directory"
                );
            }
        }
        if proposed.len() >= MAX_DISCOVERED_ROOTS {
            break;
        }
    }
    let mut roots: Vec<PathBuf> = proposed.into_iter().map(|(root, _)| root).collect();
    roots.sort();
    roots
}

/// The proposals that overlap none of `excluded` (the workspace, the application cache and its
/// private control root, every writable root): a proposal that contains or lies inside an
/// excluded root is dropped rather than failing the launch.
pub(crate) fn without_overlaps(proposed: &[PathBuf], excluded: &[PathBuf]) -> Vec<PathBuf> {
    proposed
        .iter()
        .filter(|root| {
            !excluded
                .iter()
                .any(|excluded| same_or_inside(excluded, root) || same_or_inside(root, excluded))
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .join(format!("mini-agent-toolchains-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).expect("fixture root");
            Self { root }
        }

        fn canonical(&self) -> PathBuf {
            std::fs::canonicalize(&self.root).expect("canonical fixture")
        }

        fn tool(&self, directory: &str, name: &str) -> PathBuf {
            let directory = self.canonical().join(directory);
            std::fs::create_dir_all(&directory).expect("tool directory");
            std::fs::write(directory.join(name), b"x").expect("tool file");
            directory
        }

        fn environment(&self, path_directories: &[PathBuf]) -> DiscoveryEnvironment {
            DiscoveryEnvironment {
                path: std::env::join_paths(path_directories).expect("joinable path"),
                extensions: Vec::new(),
                home: Some(self.canonical().join("home").join("me")),
                protected_roots: Vec::new(),
            }
        }
    }

    #[test]
    fn a_resolved_interpreter_proposes_its_own_directory() {
        let fixture = Fixture::new();
        let python = fixture.tool("tools/python312", "python");
        let environment = fixture.environment(std::slice::from_ref(&python));
        assert_eq!(discover(&environment), vec![python]);
    }

    #[test]
    fn a_scripts_directory_proposes_the_interpreter_root_above_it() {
        let fixture = Fixture::new();
        let scripts = fixture.tool("tools/python312/Scripts", "pip");
        let root = scripts.parent().unwrap().to_path_buf();
        let environment = fixture.environment(&[scripts]);
        assert_eq!(discover(&environment), vec![root]);
    }

    #[test]
    fn a_bin_directory_proposes_its_install_prefix_unless_it_sits_in_a_dot_directory() {
        let fixture = Fixture::new();
        let ruby_bin = fixture.tool("tools/ruby33/bin", "ruby");
        let ruby_root = ruby_bin.parent().unwrap().to_path_buf();
        assert_eq!(discover(&fixture.environment(&[ruby_bin])), vec![ruby_root]);
        let cargo_bin = fixture.tool("home/me/.cargo/bin", "cargo");
        let mut environment = fixture.environment(std::slice::from_ref(&cargo_bin));
        environment.home = Some(fixture.canonical().join("elsewhere"));
        assert_eq!(discover(&environment), vec![cargo_bin]);
    }

    #[test]
    fn nested_proposals_collapse_into_the_broader_root() {
        let fixture = Fixture::new();
        let python = fixture.tool("tools/python312", "python");
        let scripts = fixture.tool("tools/python312/Scripts", "pip");
        let environment = fixture.environment(&[scripts, python.clone()]);
        assert_eq!(discover(&environment), vec![python]);
    }

    #[test]
    fn only_listed_toolchain_commands_are_resolved() {
        let fixture = Fixture::new();
        let other = fixture.tool("tools/other", "curl");
        let environment = fixture.environment(&[other]);
        assert!(discover(&environment).is_empty());
    }

    #[test]
    fn already_readable_trees_are_not_proposed() {
        let fixture = Fixture::new();
        let node = fixture.tool("Program Files/nodejs", "node");
        let mut environment = fixture.environment(&[node]);
        environment.protected_roots = vec![fixture.canonical().join("Program Files")];
        assert!(discover(&environment).is_empty());
    }

    #[test]
    fn the_home_directory_and_its_ancestors_are_never_proposed() {
        let fixture = Fixture::new();
        let home = fixture.tool("home/me", "python");
        let mut environment = fixture.environment(std::slice::from_ref(&home));
        environment.home = Some(home);
        assert!(discover(&environment).is_empty());
        let ancestor = fixture.tool("home", "node");
        assert!(
            discover(&fixture.environment(&[ancestor])).is_empty(),
            "an ancestor of the home directory contains it"
        );
    }

    #[test]
    fn a_directory_holding_credential_folders_is_not_proposed() {
        let fixture = Fixture::new();
        let tools = fixture.tool("tools/mixed", "python");
        std::fs::create_dir(tools.join(".ssh")).unwrap();
        assert!(discover(&fixture.environment(&[tools])).is_empty());
    }

    #[test]
    fn broad_directory_names_are_not_proposed() {
        let fixture = Fixture::new();
        let roaming = fixture.tool("home/me/AppData/Roaming", "node");
        let mut environment = fixture.environment(&[roaming]);
        environment.home = None;
        assert!(discover(&environment).is_empty());
    }

    #[test]
    fn overlaps_with_excluded_roots_in_either_direction_are_dropped() {
        let fixture = Fixture::new();
        let tools = fixture.tool("work/tools", "python");
        let proposed = vec![tools.clone()];
        let inside = tools.join("project");
        assert!(
            without_overlaps(&proposed, &[inside]).is_empty(),
            "contains it"
        );
        assert!(
            without_overlaps(&proposed, &[tools.join("lib")]).is_empty(),
            "lies inside it"
        );
        assert!(without_overlaps(&proposed, std::slice::from_ref(&tools)).is_empty());
        assert_eq!(
            without_overlaps(&proposed, &[fixture.canonical().join("elsewhere")]),
            proposed
        );
    }

    #[test]
    fn oversized_trees_are_skipped() {
        let fixture = Fixture::new();
        let tools = fixture.tool("tools/big", "python");
        for index in 0..8 {
            std::fs::write(tools.join(format!("f{index}")), b"x").unwrap();
        }
        assert!(bounded_entry_count(&tools, 4) > 4);
        assert!(bounded_entry_count(&tools, 100) <= 100);
        assert!(
            judge(
                &tools,
                &fixture.environment(std::slice::from_ref(&tools)),
                100
            )
            .is_ok()
        );
        assert_eq!(
            judge(
                &tools,
                &fixture.environment(std::slice::from_ref(&tools)),
                4
            ),
            Err(Rejection::TooLarge)
        );
        assert!(
            bounded_entry_count(&tools, MAX_DISCOVERED_ROOT_ENTRIES) <= MAX_DISCOVERED_ROOT_ENTRIES
        );
    }

    #[test]
    fn the_total_entry_budget_keeps_earlier_commands_and_skips_the_rest() {
        let fixture = Fixture::new();
        let python = fixture.tool("tools/python312", "python");
        let node = fixture.tool("tools/node", "node");
        for directory in [&python, &node] {
            for index in 0..6 {
                std::fs::write(directory.join(format!("f{index}")), b"x").unwrap();
            }
        }
        let environment = fixture.environment(&[node.clone(), python.clone()]);
        // Each tree holds 7 entries: both fit in 20, only the first listed command fits in 10.
        assert_eq!(discover_within(&environment, 100, 20).len(), 2);
        assert_eq!(discover_within(&environment, 100, 10), vec![python]);
    }

    #[test]
    fn the_proposal_is_capped() {
        let fixture = Fixture::new();
        let mut directories = Vec::new();
        for (index, name) in TOOLCHAIN_COMMANDS
            .iter()
            .take(MAX_DISCOVERED_ROOTS + 3)
            .enumerate()
        {
            directories.push(fixture.tool(&format!("t{index}"), name));
        }
        let proposed = discover(&fixture.environment(&directories));
        assert_eq!(proposed.len(), MAX_DISCOVERED_ROOTS);
    }

    #[test]
    fn relative_and_empty_path_entries_are_ignored() {
        let fixture = Fixture::new();
        let python = fixture.tool("tools/python312", "python");
        let mut environment = fixture.environment(&[]);
        let relative = PathBuf::from("relative-tools");
        environment.path =
            std::env::join_paths([PathBuf::new(), relative, python.clone()]).expect("joinable");
        assert_eq!(discover(&environment), vec![python]);
    }
}
