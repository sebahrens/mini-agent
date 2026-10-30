//! Configurable read-only roots for the Linux `bwrap` general-process profile.
//!
//! The base profile mounts only the system runtime roots
//! ([`BWRAP_DEFAULT_RUNTIME_ROOTS`]). User-installed CLIs outside them —
//! Linuxbrew under `/home/linuxbrew/.linuxbrew`, `~/.cargo/bin`,
//! `~/.local/bin`, or the rustup home that `/usr/bin/cargo` proxies into — are
//! then unreachable inside the sandbox even though `PATH` still names them, so
//! the model sees `command not found` for tools the user runs on the host.
//!
//! `sandbox-toolchain-roots` lists extra read-only binds. When unset it defaults
//! to [`DEFAULT_SANDBOX_READ_ROOTS`]; setting it replaces that list. Entries
//! must be absolute or start with `~/`, are resolved per launch, and:
//!
//! * a missing entry is skipped (`--ro-bind-try`, with a debug log), so one
//!   configuration works across hosts;
//! * an entry is rejected — failing the launch closed — when it is the
//!   filesystem root, the user's home or one of its ancestors, or when it
//!   overlaps the writable workspace (and therefore its read-only Git
//!   metadata binds), the application cache, or mini-agent's private
//!   configuration, credential, or state directory.
//!
//! Every bind is read-only and precedes the writable workspace bind; nothing
//! here adds or widens a writable mount.

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

/// Runtime roots every Linux `bwrap` general-process profile mounts.
#[cfg_attr(not(feature = "hooks"), allow(dead_code))]
pub(crate) const BWRAP_DEFAULT_RUNTIME_ROOTS: [&str; 7] =
    ["/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/nix"];

/// Sandbox-internal mount points a root may never cover or sit inside: the
/// synthetic `/proc`, `/dev`, and `/sys`, the bound-workspace alias, and the
/// immutable-executable snapshot directory.
const RESERVED_SANDBOX_PATHS: [&str; 5] =
    ["/proc", "/dev", "/sys", "/workspace", "/run/mini-agent"];

/// Default `sandbox-toolchain-roots` when the key is unset. Homebrew-on-Linux,
/// Cargo-installed binaries, user-local binaries, and the rustup toolchain
/// home those proxies (and Debian's `/usr/bin/cargo`) need. None of these
/// hold credentials; `~/.cargo` itself is deliberately not listed because it
/// contains `credentials.toml`.
pub(crate) const DEFAULT_SANDBOX_READ_ROOTS: [&str; 4] = [
    "/home/linuxbrew",
    "~/.cargo/bin",
    "~/.local/bin",
    "~/.rustup",
];

/// The default list as configuration values.
pub(crate) fn default_sandbox_read_roots() -> Vec<PathBuf> {
    DEFAULT_SANDBOX_READ_ROOTS
        .iter()
        .map(PathBuf::from)
        .collect()
}

/// One validated read-only bind: the source and every mount point it is
/// exposed at (the canonical path, plus the configured spelling when that
/// reaches the same tree through a symlink).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BwrapReadRoot {
    pub(crate) source: PathBuf,
    pub(crate) mount_points: Vec<PathBuf>,
    /// `false` for a configured root that did not exist at resolution time;
    /// it is emitted as `--ro-bind-try` so bubblewrap skips it while absent.
    pub(crate) present: bool,
}

/// The validated read-only roots for one launch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BwrapReadRoots {
    roots: Vec<BwrapReadRoot>,
}

impl BwrapReadRoots {
    pub(crate) fn none() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn roots(&self) -> &[BwrapReadRoot] {
        &self.roots
    }

    /// Append the read-only bind arguments. A present source was
    /// canonicalized during resolution, so its `--ro-bind` fails closed if it
    /// disappears before launch; an absent root uses `--ro-bind-try`.
    pub(crate) fn apply(&self, command: &mut super::Command) {
        for root in &self.roots {
            let flag = if root.present {
                "--ro-bind"
            } else {
                "--ro-bind-try"
            };
            for mount_point in &root.mount_points {
                command.arg(flag).arg(&root.source).arg(mount_point);
            }
        }
    }

    /// `PATH` for the sandboxed process: the host value, followed by every
    /// mounted `bin`/`sbin` root it does not already name, so a bound
    /// `~/.cargo/bin` is reachable even when the host `PATH` omits it.
    pub(crate) fn sandbox_path(&self, host_path: Option<&OsStr>) -> Option<OsString> {
        let mut entries: Vec<PathBuf> = host_path
            .map(|path| std::env::split_paths(path).collect())
            .unwrap_or_default();
        let original_len = entries.len();
        for root in self.roots.iter().filter(|root| root.present) {
            for mount_point in &root.mount_points {
                let is_bin = matches!(
                    mount_point.file_name().and_then(OsStr::to_str),
                    Some("bin" | "sbin")
                );
                if is_bin && !entries.iter().any(|entry| same_path(entry, mount_point)) {
                    entries.push(mount_point.clone());
                }
            }
        }
        if entries.len() == original_len {
            return host_path.map(OsStr::to_os_string);
        }
        std::env::join_paths(entries).ok()
    }

    /// Human-readable effective list for `--print-config`.
    pub(crate) fn describe(&self) -> String {
        if self.roots.is_empty() {
            return "none".to_string();
        }
        self.roots
            .iter()
            .map(|root| {
                let mut text = root.source.display().to_string();
                for alias in root.mount_points.iter().filter(|m| **m != root.source) {
                    text.push_str(&format!(" (also at {})", alias.display()));
                }
                if !root.present {
                    text.push_str(" (absent; skipped)");
                }
                text
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    left.components().eq(right.components())
}

/// Paths a read root must never overlap. Entries are canonical where the
/// path exists; missing private directories are compared lexically.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProtectedPaths {
    pub(crate) home: Option<PathBuf>,
    /// Writable binds: the workspace and the application cache.
    pub(crate) writable: Vec<PathBuf>,
    /// mini-agent's private configuration, credential, and state directories.
    pub(crate) private: Vec<PathBuf>,
}

/// Resolve and validate the configured read-only roots for one launch.
pub(crate) fn resolve(
    configured_roots: &[PathBuf],
    protected: &ProtectedPaths,
) -> Result<BwrapReadRoots, String> {
    let mut roots: Vec<BwrapReadRoot> = Vec::new();
    for configured in configured_roots {
        let Some(lexical) = expand_root(configured, protected.home.as_deref())? else {
            tracing::debug!(
                "sandbox: skipping sandbox-toolchain-roots entry {} because no home directory is known",
                configured.display()
            );
            continue;
        };
        let check = |path: &Path| match verdict(path, protected) {
            Verdict::Bind => Ok(true),
            Verdict::Redundant => {
                tracing::debug!(
                    "sandbox: sandbox-toolchain-roots entry {} is already inside a writable bind; skipped",
                    configured.display()
                );
                Ok(false)
            }
            Verdict::Refuse(reason) => Err(format!(
                "sandbox: refusing sandbox-toolchain-roots entry {}: {reason}",
                configured.display()
            )),
        };
        if !check(&lexical)? {
            continue;
        }
        match std::fs::canonicalize(&lexical) {
            Ok(source) => {
                if !check(&source)? {
                    continue;
                }
                push_root(&mut roots, source, lexical, true);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(
                    "sandbox: sandbox-toolchain-roots entry {} does not exist; skipped (--ro-bind-try)",
                    lexical.display()
                );
                push_root(&mut roots, lexical.clone(), lexical, false);
            }
            Err(error) => {
                return Err(format!(
                    "sandbox: failed to resolve sandbox-toolchain-roots entry {}: {error}",
                    lexical.display()
                ));
            }
        }
    }

    // A root nested inside another present root is already visible through it.
    let outer: Vec<PathBuf> = roots
        .iter()
        .filter(|root| root.present)
        .map(|root| root.source.clone())
        .collect();
    roots.retain(|root| {
        !outer
            .iter()
            .any(|other| other != &root.source && root.source.starts_with(other))
    });
    Ok(BwrapReadRoots { roots })
}

fn push_root(roots: &mut Vec<BwrapReadRoot>, source: PathBuf, lexical: PathBuf, present: bool) {
    let mut mount_points = vec![source.clone()];
    if lexical != source {
        mount_points.push(lexical);
    }
    if let Some(existing) = roots.iter_mut().find(|root| root.source == source) {
        for mount_point in mount_points {
            if !existing.mount_points.contains(&mount_point) {
                existing.mount_points.push(mount_point);
            }
        }
    } else {
        roots.push(BwrapReadRoot {
            source,
            mount_points,
            present,
        });
    }
}

/// Expand a leading `~` against the real home directory and require an
/// absolute, `..`-free result. `None` when the entry is home-relative but no
/// home directory is known, so the default list stays usable without one.
pub(crate) fn expand_root(
    configured: &Path,
    home: Option<&Path>,
) -> Result<Option<PathBuf>, String> {
    let mut components = configured.components();
    let expanded = match components.next() {
        Some(Component::Normal(first)) if first == "~" => {
            let Some(home) = home else {
                return Ok(None);
            };
            home.join(components.as_path())
        }
        _ => configured.to_path_buf(),
    };
    if !expanded.is_absolute() {
        return Err(format!(
            "sandbox: sandbox-toolchain-roots entry {} must be an absolute path or start with ~/",
            configured.display()
        ));
    }
    if expanded
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!(
            "sandbox: sandbox-toolchain-roots entry {} must not contain `..`",
            configured.display()
        ));
    }
    Ok(Some(expanded.components().collect()))
}

enum Verdict {
    Bind,
    /// Already visible (and writable) through the workspace or cache bind;
    /// a read-only bind would shadow writable state, so it is skipped.
    Redundant,
    Refuse(&'static str),
}

/// Whether `root` may be bound.
fn verdict(root: &Path, protected: &ProtectedPaths) -> Verdict {
    if root.parent().is_none() {
        return Verdict::Refuse("the filesystem root is never exposed");
    }
    if let Some(home) = &protected.home
        && home.starts_with(root)
    {
        return Verdict::Refuse("the home directory and its ancestors are never exposed wholesale");
    }
    for reserved in RESERVED_SANDBOX_PATHS.iter().map(Path::new) {
        if reserved.starts_with(root) || root.starts_with(reserved) {
            return Verdict::Refuse(
                "it overlaps a sandbox-internal mount (/proc, /dev, /sys, /workspace, or /run/mini-agent)",
            );
        }
    }
    for private in &protected.private {
        if private.starts_with(root) || root.starts_with(private) {
            return Verdict::Refuse(
                "it overlaps mini-agent's private configuration, credential, or state directory",
            );
        }
    }
    for writable in &protected.writable {
        if writable.starts_with(root) {
            return Verdict::Refuse(
                "it contains the writable workspace (including its protected Git metadata) or the application cache",
            );
        }
    }
    if protected
        .writable
        .iter()
        .any(|writable| root.starts_with(writable))
    {
        return Verdict::Redundant;
    }
    Verdict::Bind
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_tree(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "mini-agent-read-roots-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root.canonicalize().unwrap()
    }

    fn protected(home: &Path, workspace: &Path) -> ProtectedPaths {
        ProtectedPaths {
            home: Some(home.to_path_buf()),
            writable: vec![workspace.to_path_buf()],
            private: vec![home.join(".config/mini-agent")],
        }
    }

    fn args(roots: &BwrapReadRoots) -> Vec<String> {
        let mut command = tokio::process::Command::new("bwrap");
        roots.apply(&mut command);
        command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn default_roots_cover_linuxbrew_cargo_local_bin_and_rustup_without_cargo_credentials() {
        assert_eq!(
            default_sandbox_read_roots(),
            [
                PathBuf::from("/home/linuxbrew"),
                PathBuf::from("~/.cargo/bin"),
                PathBuf::from("~/.local/bin"),
                PathBuf::from("~/.rustup"),
            ]
        );
        assert!(
            !DEFAULT_SANDBOX_READ_ROOTS
                .iter()
                .any(|root| *root == "~/.cargo" || root.contains("credentials")),
            "Cargo's credential file must never be bound by default"
        );
    }

    #[test]
    fn default_roots_expand_home_and_absent_entries_use_try_semantics() {
        let base = temp_tree("defaults");
        let home = base.join("home/user");
        let workspace = home.join("project");
        std::fs::create_dir_all(home.join(".cargo/bin")).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();

        let roots = resolve(&default_sandbox_read_roots(), &protected(&home, &workspace)).unwrap();
        let cargo_bin = home.join(".cargo/bin").display().to_string();
        let local_bin = home.join(".local/bin").display().to_string();
        let rustup = home.join(".rustup").display().to_string();
        let brew_present = Path::new("/home/linuxbrew").exists();
        let brew_flag = if brew_present {
            "--ro-bind"
        } else {
            "--ro-bind-try"
        };
        let brew = std::fs::canonicalize("/home/linuxbrew")
            .unwrap_or_else(|_| PathBuf::from("/home/linuxbrew"))
            .display()
            .to_string();
        let mut expected = vec![brew_flag.to_string(), brew.clone(), brew];
        if brew_present && Path::new("/home/linuxbrew") != Path::new(&expected[1]) {
            expected.extend([
                "--ro-bind".into(),
                expected[1].clone(),
                "/home/linuxbrew".into(),
            ]);
        }
        expected.extend([
            "--ro-bind".to_string(),
            cargo_bin.clone(),
            cargo_bin,
            "--ro-bind-try".to_string(),
            local_bin.clone(),
            local_bin,
            "--ro-bind-try".to_string(),
            rustup.clone(),
            rustup,
        ]);
        assert_eq!(args(&roots), expected);
        assert!(
            args(&roots)
                .iter()
                .any(|arg| arg.starts_with("/home/linuxbrew"))
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn sandbox_path_appends_mounted_bin_roots_missing_from_host_path() {
        let base = temp_tree("path");
        let home = base.join("home/user");
        let workspace = home.join("project");
        let cargo_bin = home.join(".cargo/bin");
        std::fs::create_dir_all(&cargo_bin).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        let roots = resolve(
            &[PathBuf::from("~/.cargo/bin"), PathBuf::from("~/.local/bin")],
            &protected(&home, &workspace),
        )
        .unwrap();

        let path = roots
            .sandbox_path(Some(OsStr::new("/usr/bin:/bin")))
            .unwrap();
        assert_eq!(
            path,
            OsString::from(format!("/usr/bin:/bin:{}", cargo_bin.display())),
            "only present bin roots are appended"
        );
        let already = std::env::join_paths([cargo_bin.as_path(), Path::new("/usr/bin")]).unwrap();
        assert_eq!(roots.sandbox_path(Some(&already)).unwrap(), already);
        assert_eq!(
            BwrapReadRoots::none().sandbox_path(Some(OsStr::new("/usr/bin"))),
            Some(OsString::from("/usr/bin"))
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn nested_and_duplicate_roots_collapse_to_the_outer_present_root() {
        let base = temp_tree("nested");
        let home = base.join("home/user");
        let rustup = home.join(".rustup");
        let workspace = home.join("project");
        for dir in [&rustup.join("toolchains"), &workspace] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let roots = resolve(
            &[
                rustup.join("toolchains"),
                PathBuf::from("~/.rustup"),
                rustup.clone(),
            ],
            &protected(&home, &workspace),
        )
        .unwrap();
        assert_eq!(
            roots.roots(),
            &[BwrapReadRoot {
                source: rustup.clone(),
                mount_points: vec![rustup],
                present: true,
            }]
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_root_is_also_mounted_at_its_configured_spelling() {
        let base = temp_tree("symlink");
        let home = base.join("home/user");
        let workspace = home.join("project");
        let target = base.join("opt/sdk-1.2");
        for dir in [&workspace, &target] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let link = base.join("opt/sdk");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let roots = resolve(std::slice::from_ref(&link), &protected(&home, &workspace)).unwrap();
        assert_eq!(
            args(&roots),
            [
                "--ro-bind".to_string(),
                target.display().to_string(),
                target.display().to_string(),
                "--ro-bind".to_string(),
                target.display().to_string(),
                link.display().to_string(),
            ]
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_into_a_protected_path_is_refused() {
        let base = temp_tree("symlink-escape");
        let home = base.join("home/user");
        let workspace = home.join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let link = base.join("opt/innocent");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&home, &link).unwrap();
        let error =
            resolve(std::slice::from_ref(&link), &protected(&home, &workspace)).unwrap_err();
        assert!(error.contains("home directory"), "{error}");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn roots_that_weaken_the_boundary_fail_closed() {
        let base = temp_tree("reject");
        let home = base.join("home/user");
        let workspace = home.join("project");
        let private = home.join(".config/mini-agent");
        for dir in [&workspace.join(".git"), &private] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let protected = protected(&home, &workspace);
        for (entry, expected) in [
            (PathBuf::from("/"), "filesystem root"),
            (home.clone(), "home directory"),
            (base.join("home"), "home directory"),
            (PathBuf::from("~"), "home directory"),
            (PathBuf::from("~/"), "home directory"),
            (workspace.clone(), "writable workspace"),
            (home.join(".config"), "private configuration"),
            (private.clone(), "private configuration"),
            (PathBuf::from("/dev/sda"), "sandbox-internal"),
            (PathBuf::from("/proc"), "sandbox-internal"),
            (PathBuf::from("/run"), "sandbox-internal"),
            (PathBuf::from("/workspace"), "sandbox-internal"),
            (PathBuf::from("relative/tool"), "absolute path"),
            (PathBuf::from("$HOME/bin"), "absolute path"),
            (base.join("opt/../home"), "must not contain"),
        ] {
            let error = resolve(std::slice::from_ref(&entry), &protected).unwrap_err();
            assert!(error.contains(expected), "{}: {error}", entry.display());
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn roots_inside_the_writable_workspace_are_skipped_not_shadowed() {
        let base = temp_tree("inside-workspace");
        // Running in $HOME makes the home-relative defaults workspace-internal.
        let home = base.join("home/user");
        std::fs::create_dir_all(home.join(".cargo/bin")).unwrap();
        std::fs::create_dir_all(home.join(".git")).unwrap();
        let protected = protected(&home, &home);
        let roots = resolve(
            &[
                PathBuf::from("~/.cargo/bin"),
                PathBuf::from("~/.git"),
                PathBuf::from("~/.missing/bin"),
            ],
            &protected,
        )
        .unwrap();
        assert!(roots.roots().is_empty(), "{roots:?}");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn home_relative_defaults_are_skipped_without_a_home_directory() {
        let roots = resolve(
            &default_sandbox_read_roots(),
            &ProtectedPaths {
                home: None,
                ..ProtectedPaths::default()
            },
        )
        .unwrap();
        let args = args(&roots);
        assert!(args.iter().any(|arg| arg == "/home/linuxbrew"), "{args:?}");
        assert!(!args.iter().any(|arg| arg.contains(".cargo")), "{args:?}");
    }

    #[test]
    fn empty_configuration_binds_nothing() {
        let roots = resolve(&[], &ProtectedPaths::default()).unwrap();
        assert_eq!(roots, BwrapReadRoots::none());
        assert_eq!(roots.describe(), "none");
        assert!(args(&roots).is_empty());
    }
}
