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
//! * `PATH` entries under the Windows directory, and trees an inheritable
//!   `ALL APPLICATION PACKAGES` ACE already makes readable (most of `Program Files`), are skipped;
//!   an administrator-owned directory whose DACL the user cannot change is skipped at grant time;
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
    /// Trees never proposed (the Windows directory: always readable, never ours to change).
    pub(crate) protected_roots: Vec<PathBuf>,
    /// Whether every application package already reads and executes the tree under a directory
    /// (an inheritable `ALL APPLICATION PACKAGES` ACE), so no grant is needed.
    pub(crate) already_readable: Option<fn(&Path) -> bool>,
    /// mini-agent's own configuration, credential, data, state and cache directories: a proposal
    /// may neither contain nor lie inside one.
    pub(crate) excluded_roots: Vec<PathBuf>,
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
            already_readable: None,
            excluded_roots: Vec::new(),
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

/// Whether `prefix` looks like a tool install (it has `lib`, `Lib`, `include` or `libexec`) and
/// holds no repository or secret marker (`.git`, `.env`) within two levels. Only such a prefix is
/// granted in place of its `bin`/`Scripts` directory: `D:\\dev\\bin\\make.exe` must not expose `D:\\dev`.
fn looks_like_install(prefix: &Path) -> bool {
    const LIBRARY_DIRECTORIES: [&str; 4] = ["lib", "include", "libexec", "lib64"];
    const MARKERS: [&str; 2] = [".git", ".env"];
    let Ok(entries) = std::fs::read_dir(prefix) else {
        return false;
    };
    let mut has_library = false;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if MARKERS.contains(&name.as_str()) {
            return false;
        }
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            if LIBRARY_DIRECTORIES.contains(&name.as_str()) {
                has_library = true;
            }
            let Ok(children) = std::fs::read_dir(entry.path()) else {
                continue;
            };
            for child in children.flatten() {
                let child = child.file_name().to_string_lossy().to_ascii_lowercase();
                if MARKERS.contains(&child.as_str()) {
                    return false;
                }
            }
        }
    }
    has_library
}

/// The directory to propose for a resolved executable: its own directory, except that a
/// `Scripts` directory (pip, pytest, ...) or a `bin` directory of an install prefix (Go, Ruby,
/// a JDK, MinGW) proposes the prefix above it, where the standard library lives, when the prefix
/// [looks like an install](looks_like_install); otherwise only the `bin`/`Scripts` directory is
/// proposed. A `bin` under a dot-directory (`.cargo\bin`, `.local\bin`) stays as it is:
/// dot-directories also hold configuration and credentials.
fn proposed_directory(executable: &Path) -> Option<PathBuf> {
    let directory = executable.parent()?;
    let prefix = directory.parent().map(Path::to_path_buf);
    let promote = match lowered_name(directory).as_deref() {
        Some("scripts") => true,
        Some("bin") => prefix
            .as_deref()
            .and_then(lowered_name)
            .is_some_and(|name| !name.starts_with('.')),
        _ => false,
    };
    match prefix {
        Some(prefix) if promote && looks_like_install(&prefix) => Some(prefix),
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
    ContainsLinks(String),
    OverlapsExcludedRoot,
}

fn credential_child_present(directory: &Path) -> bool {
    CREDENTIAL_DIRECTORIES
        .iter()
        .any(|name| std::fs::symlink_metadata(directory.join(name)).is_ok())
}

/// What a bounded walk below a root found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreeScan {
    /// Entries seen (capped at `limit + 1`).
    pub(crate) entries: usize,
    /// A directory symbolic link or junction (or one that cannot be resolved) exists below the
    /// root: an inheritable grant could follow it out of the tree, so such a root is never
    /// proposed. Links to files do not leave the tree and are allowed. Holds the entry's name.
    pub(crate) link: Option<String>,
}

/// Walk `root` without following links, stopping at `limit + 1` entries.
pub(crate) fn scan_tree(root: &Path, limit: usize) -> TreeScan {
    let mut scan = TreeScan {
        entries: 0,
        link: None,
    };
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            scan.entries += 1;
            if scan.entries > limit {
                return scan;
            }
            match entry.file_type() {
                Ok(kind) if kind.is_symlink() => {
                    if std::fs::metadata(entry.path()).is_ok_and(|target| !target.is_dir()) {
                        continue;
                    }
                    scan.link = Some(entry.file_name().to_string_lossy().into_owned());
                    return scan;
                }
                Ok(kind) if kind.is_dir() => pending.push(entry.path()),
                _ => {}
            }
        }
    }
    scan
}

/// Count the entries below `root` without following links, stopping at `limit + 1`.
pub(crate) fn bounded_entry_count(root: &Path, limit: usize) -> usize {
    scan_tree(root, limit).entries
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
        || environment
            .already_readable
            .is_some_and(|readable| readable(directory))
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
    if environment
        .excluded_roots
        .iter()
        .any(|excluded| same_or_inside(excluded, directory) || same_or_inside(directory, excluded))
    {
        return Err(Rejection::OverlapsExcludedRoot);
    }
    let scan = scan_tree(directory, entry_limit);
    if let Some(link) = scan.link {
        return Err(Rejection::ContainsLinks(link));
    }
    if scan.entries > entry_limit {
        return Err(Rejection::TooLarge);
    }
    Ok(scan.entries)
}

/// A proposed read/execute root and the command words that make a launch need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Toolchain {
    pub(crate) root: PathBuf,
    /// Lower-case command stems (`python`, `pip`, `pytest`, ...) of the executables in the
    /// `PATH` directory the root was resolved from, and of the root itself.
    triggers: Vec<String>,
}

/// Upper bound on trigger words kept per toolchain.
const MAX_TRIGGERS: usize = 1024;

fn executable_stem(name: &str, extensions: &[String]) -> Option<String> {
    let lowered = name.to_ascii_lowercase();
    for extension in extensions {
        if let Some(stem) = lowered.strip_suffix(&extension.to_ascii_lowercase())
            && !stem.is_empty()
        {
            return Some(stem.to_string());
        }
    }
    // Extensionless launchers (`npm`, `pip3`) are commands too.
    (!lowered.contains('.') && !lowered.is_empty()).then_some(lowered)
}

fn collect_triggers(directory: &Path, extensions: &[String], into: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        if into.len() >= MAX_TRIGGERS {
            return;
        }
        if entry.file_type().is_ok_and(|kind| kind.is_file())
            && let Some(stem) = executable_stem(&entry.file_name().to_string_lossy(), extensions)
            && !into.contains(&stem)
        {
            into.push(stem);
        }
    }
}

/// Command words of a shell script: split at whitespace and shell punctuation, reduced to the
/// final path component without its executable suffix, lower-cased.
pub(crate) fn command_words(script: &str) -> Vec<String> {
    script
        .split(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    ';' | '&'
                        | '|'
                        | '('
                        | ')'
                        | '<'
                        | '>'
                        | '"'
                        | '\''
                        | '='
                        | ','
                        | '`'
                        | '{'
                        | '}'
                )
        })
        .filter(|word| !word.is_empty())
        .map(|word| {
            let trimmed = word.trim_end_matches(['\\', '/']);
            let leaf = trimmed.rsplit(['\\', '/']).next().unwrap_or(trimmed);
            let lowered = leaf.to_ascii_lowercase();
            [".exe", ".cmd", ".bat", ".com", ".ps1"]
                .iter()
                .find_map(|suffix| lowered.strip_suffix(suffix))
                .map(str::to_string)
                .unwrap_or(lowered)
        })
        .collect()
}

/// The roots of the toolchains `script` names: a launch only pays for the grants its command
/// can use. A command that reaches a tool indirectly (`make test` running Python) names none of
/// them; the failure note then points at `windows-appcontainer-read-roots`.
pub(crate) fn mentioned_roots(toolchains: &[Toolchain], script: &str) -> Vec<PathBuf> {
    let words = command_words(script);
    toolchains
        .iter()
        .filter(|toolchain| {
            toolchain
                .triggers
                .iter()
                .any(|trigger| words.iter().any(|word| word == trigger))
        })
        .map(|toolchain| toolchain.root.clone())
        .collect()
}

/// The read/execute roots to propose, deduplicated and without nested entries. Each root is a
/// canonical directory; nothing here touches an ACL.
pub(crate) fn discover(environment: &DiscoveryEnvironment) -> Vec<Toolchain> {
    discover_with_report(environment).0
}

/// [`discover`] plus one `command: reason` line for every resolved command whose directory was
/// not proposed (shown by `--doctor`, so a missing grant is explainable).
pub(crate) fn discover_with_report(
    environment: &DiscoveryEnvironment,
) -> (Vec<Toolchain>, Vec<String>) {
    let mut skipped = Vec::new();
    let toolchains = discover_within(
        environment,
        MAX_DISCOVERED_ROOT_ENTRIES,
        MAX_DISCOVERED_TOTAL_ENTRIES,
        &mut skipped,
    );
    (toolchains, skipped)
}

fn discover_within(
    environment: &DiscoveryEnvironment,
    root_limit: usize,
    total_limit: usize,
    skipped: &mut Vec<String>,
) -> Vec<Toolchain> {
    let mut proposed: Vec<(Toolchain, usize)> = Vec::new();
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
            .any(|(known, _)| same_or_inside(&directory, &known.root))
        {
            continue;
        }
        let verdict = judge(&directory, environment, root_limit).and_then(|entries| {
            // A broader accepted root replaces the narrower ones it contains.
            let replaced: usize = proposed
                .iter()
                .filter(|(known, _)| same_or_inside(&known.root, &directory))
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
                // A broader root also answers to the commands of the narrower ones it absorbs.
                let mut triggers = vec![(*name).to_string()];
                for (known, _) in proposed
                    .iter()
                    .filter(|(known, _)| same_or_inside(&known.root, &directory))
                {
                    triggers.extend(known.triggers.iter().cloned());
                }
                proposed.retain(|(known, _)| !same_or_inside(&known.root, &directory));
                if let Some(path_directory) = executable.parent() {
                    collect_triggers(path_directory, &environment.extensions, &mut triggers);
                }
                collect_triggers(&directory, &environment.extensions, &mut triggers);
                triggers.sort();
                triggers.dedup();
                proposed.push((
                    Toolchain {
                        root: directory,
                        triggers,
                    },
                    entries,
                ));
            }
            Err(reason) => {
                skipped.push(format!("{name}: {reason:?}"));
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
    let mut toolchains: Vec<Toolchain> = proposed
        .into_iter()
        .map(|(toolchain, _)| toolchain)
        .collect();
    toolchains.sort_by(|left, right| left.root.cmp(&right.root));
    toolchains
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

    fn roots(environment: &DiscoveryEnvironment) -> Vec<PathBuf> {
        discover(environment)
            .into_iter()
            .map(|toolchain| toolchain.root)
            .collect()
    }

    fn roots_within(
        environment: &DiscoveryEnvironment,
        root_limit: usize,
        total_limit: usize,
    ) -> Vec<PathBuf> {
        discover_within(environment, root_limit, total_limit, &mut Vec::new())
            .into_iter()
            .map(|toolchain| toolchain.root)
            .collect()
    }

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
                already_readable: None,
                excluded_roots: Vec::new(),
            }
        }
    }

    #[test]
    fn a_resolved_interpreter_proposes_its_own_directory() {
        let fixture = Fixture::new();
        let python = fixture.tool("tools/python312", "python");
        let environment = fixture.environment(std::slice::from_ref(&python));
        assert_eq!(roots(&environment), vec![python]);
    }

    #[test]
    fn a_scripts_directory_proposes_the_interpreter_root_above_it() {
        let fixture = Fixture::new();
        let scripts = fixture.tool("tools/python312/Scripts", "pip");
        let root = scripts.parent().unwrap().to_path_buf();
        std::fs::create_dir(root.join("Lib")).unwrap();
        let environment = fixture.environment(&[scripts]);
        assert_eq!(roots(&environment), vec![root]);
    }

    #[test]
    fn a_bin_directory_proposes_its_install_prefix_unless_it_sits_in_a_dot_directory() {
        let fixture = Fixture::new();
        let ruby_bin = fixture.tool("tools/ruby33/bin", "ruby");
        let ruby_root = ruby_bin.parent().unwrap().to_path_buf();
        std::fs::create_dir(ruby_root.join("lib")).unwrap();
        assert_eq!(roots(&fixture.environment(&[ruby_bin])), vec![ruby_root]);
        let cargo_bin = fixture.tool("home/me/.cargo/bin", "cargo");
        let mut environment = fixture.environment(std::slice::from_ref(&cargo_bin));
        environment.home = Some(fixture.canonical().join("elsewhere"));
        assert_eq!(roots(&environment), vec![cargo_bin]);
    }

    #[test]
    fn a_bin_directory_of_a_non_install_prefix_grants_only_the_bin_directory() {
        let fixture = Fixture::new();
        // `dev/bin/make` with no library directory: the parent is a project area, not an install.
        let bin = fixture.tool("dev/bin", "make");
        std::fs::create_dir(bin.parent().unwrap().join("notes")).unwrap();
        assert_eq!(
            roots(&fixture.environment(std::slice::from_ref(&bin))),
            vec![bin]
        );
        // An install-shaped prefix holding a repository marker is not promoted either.
        let bin = fixture.tool("tools/ruby/bin", "ruby");
        let prefix = bin.parent().unwrap().to_path_buf();
        std::fs::create_dir(prefix.join("lib")).unwrap();
        std::fs::create_dir_all(prefix.join("share").join(".git")).unwrap();
        assert_eq!(
            roots(&fixture.environment(std::slice::from_ref(&bin))),
            vec![bin]
        );
        std::fs::remove_dir(prefix.join("share").join(".git")).unwrap();
        assert_eq!(
            roots(&fixture.environment(&[prefix.join("bin")])),
            vec![prefix]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_tree_containing_a_link_is_never_proposed() {
        let fixture = Fixture::new();
        let tools = fixture.tool("tools/py", "python");
        std::os::unix::fs::symlink(fixture.canonical(), tools.join("persist")).unwrap();
        assert!(roots(&fixture.environment(&[tools])).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_a_file_does_not_leave_the_tree_and_is_allowed() {
        let fixture = Fixture::new();
        let tools = fixture.tool("tools/py", "python");
        std::os::unix::fs::symlink(tools.join("python"), tools.join("python3")).unwrap();
        assert_eq!(
            roots(&fixture.environment(std::slice::from_ref(&tools))),
            vec![tools]
        );
    }

    #[test]
    fn mini_agents_own_directories_are_never_proposed() {
        let fixture = Fixture::new();
        let tools = fixture.tool("tools/py", "python");
        let mut environment = fixture.environment(std::slice::from_ref(&tools));
        environment.excluded_roots = vec![tools.join("config")];
        assert!(
            roots(&environment).is_empty(),
            "contains the config directory"
        );
        environment.excluded_roots = vec![fixture.canonical()];
        assert!(
            roots(&environment).is_empty(),
            "inside the application directory"
        );
        environment.excluded_roots = vec![fixture.canonical().join("elsewhere")];
        assert_eq!(roots(&environment), vec![tools]);
    }

    #[test]
    fn nested_proposals_collapse_into_the_broader_root() {
        let fixture = Fixture::new();
        let python = fixture.tool("tools/python312", "python");
        let scripts = fixture.tool("tools/python312/Scripts", "pip");
        let environment = fixture.environment(&[scripts, python.clone()]);
        assert_eq!(roots(&environment), vec![python]);
    }

    #[test]
    fn only_listed_toolchain_commands_are_resolved() {
        let fixture = Fixture::new();
        let other = fixture.tool("tools/other", "curl");
        let environment = fixture.environment(&[other]);
        assert!(roots(&environment).is_empty());
    }

    #[test]
    fn protected_and_already_readable_trees_are_not_proposed() {
        let fixture = Fixture::new();
        let windows = fixture.tool("Windows/tools", "perl");
        let node = fixture.tool("Program Files/nodejs", "node");
        let mut environment = fixture.environment(&[windows, node.clone()]);
        environment.protected_roots = vec![fixture.canonical().join("Windows")];
        // An explicit all-packages ACE keeps the readable tree out; without one it is proposed.
        assert_eq!(roots(&environment), vec![node.clone()]);
        environment.already_readable = Some(|path| path.ends_with("nodejs"));
        assert!(roots(&environment).is_empty());
    }

    #[test]
    fn the_home_directory_and_its_ancestors_are_never_proposed() {
        let fixture = Fixture::new();
        let home = fixture.tool("home/me", "python");
        let mut environment = fixture.environment(std::slice::from_ref(&home));
        environment.home = Some(home);
        assert!(roots(&environment).is_empty());
        let ancestor = fixture.tool("home", "node");
        assert!(
            roots(&fixture.environment(&[ancestor])).is_empty(),
            "an ancestor of the home directory contains it"
        );
    }

    #[test]
    fn a_directory_holding_credential_folders_is_not_proposed() {
        let fixture = Fixture::new();
        let tools = fixture.tool("tools/mixed", "python");
        std::fs::create_dir(tools.join(".ssh")).unwrap();
        assert!(roots(&fixture.environment(&[tools])).is_empty());
    }

    #[test]
    fn broad_directory_names_are_not_proposed() {
        let fixture = Fixture::new();
        let roaming = fixture.tool("home/me/AppData/Roaming", "node");
        let mut environment = fixture.environment(&[roaming]);
        environment.home = None;
        assert!(roots(&environment).is_empty());
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
        assert_eq!(roots_within(&environment, 100, 20).len(), 2);
        assert_eq!(roots_within(&environment, 100, 10), vec![python]);
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
        let proposed = roots(&fixture.environment(&directories));
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
        assert_eq!(roots(&environment), vec![python]);
    }

    #[test]
    fn command_words_reduce_to_lower_case_command_stems() {
        assert_eq!(
            command_words(
                "C:\\Python312\\Python.EXE -m pytest tests/ && npm.cmd test; (node x.js)"
            ),
            [
                "python", "-m", "pytest", "tests", "npm", "test", "node", "x.js"
            ]
        );
        assert_eq!(
            command_words("FOO=1 ./run.sh|tee"),
            ["foo", "1", "run.sh", "tee"]
        );
        assert!(command_words("   ").is_empty());
    }

    #[test]
    fn only_the_toolchains_a_command_names_are_granted() {
        let fixture = Fixture::new();
        let python = fixture.tool("tools/python312", "python");
        std::fs::write(python.join("pytest"), b"x").unwrap();
        let node = fixture.tool("tools/node", "node");
        let environment = fixture.environment(&[python.clone(), node.clone()]);
        let toolchains = discover(&environment);
        assert_eq!(toolchains.len(), 2);
        assert_eq!(
            mentioned_roots(&toolchains, "Get-ChildItem"),
            Vec::<PathBuf>::new()
        );
        assert_eq!(
            mentioned_roots(&toolchains, "python -V"),
            vec![python.clone()]
        );
        // A tool that only lives in the same directory also names its toolchain.
        assert_eq!(
            mentioned_roots(&toolchains, "pytest -x"),
            vec![python.clone()]
        );
        let mut both = mentioned_roots(&toolchains, "node a.js; python b.py");
        both.sort();
        let mut expected = vec![python, node];
        expected.sort();
        assert_eq!(both, expected);
    }
}
