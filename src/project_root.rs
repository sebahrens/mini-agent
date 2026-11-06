//! Project-root discovery from a subdirectory.
//!
//! By default the launch directory is the workspace: project assets
//! (`.mcp.json`, agents, skills, `AGENTS.md`/`CLAUDE.md`, the memory slug) are
//! read only from it, which is a deliberate trust boundary. Two things here
//! soften the surprise of starting in `packages/app` of a repository:
//!
//! * a startup notice naming the assets at the repository root that were not
//!   loaded, and
//! * the opt-in `project_root_discovery` setting (user config or `--config`,
//!   never a project file), which moves the session to the nearest `.git`
//!   ancestor so the whole session (workspace binding, trust prompts bound to
//!   that root's file contents, project assets, memory) is exactly the one a
//!   launch from the repository root would have.
use std::path::{Path, PathBuf};

/// Project-scoped assets looked for at an ancestor root, as `(label, path)`.
const ASSETS: &[(&str, &str)] = &[
    (".mcp.json", ".mcp.json"),
    ("AGENTS.md", "AGENTS.md"),
    ("CLAUDE.md", "CLAUDE.md"),
    (".mini-agent/config.toml", ".mini-agent/config.toml"),
    (".mini-agent/agents", ".mini-agent/agents"),
    (".claude/agents", ".claude/agents"),
    (".mini-agent/skills", ".mini-agent/skills"),
    (".claude/skills", ".claude/skills"),
    (".agents/skills", ".agents/skills"),
];

/// The nearest ancestor of `start` (excluding `start`) that holds a `.git`
/// entry, or `None` when `start` is itself a repository root, is not in a
/// repository, or the candidate is too broad to be a project (the filesystem
/// root, the home directory or one of its parents). A symlinked `.git` does
/// not count.
pub(crate) fn enclosing_git_root(start: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let start = start.canonicalize().ok()?;
    let home = home.and_then(|home| home.canonicalize().ok());
    let is_repo = |dir: &Path| {
        std::fs::symlink_metadata(dir.join(".git"))
            .is_ok_and(|metadata| !metadata.file_type().is_symlink())
    };
    if is_repo(&start) {
        return None;
    }
    start.ancestors().skip(1).find_map(|dir| {
        if !is_repo(dir) {
            return None;
        }
        let too_broad = dir.parent().is_none()
            || home
                .as_ref()
                .is_some_and(|home| home == dir || home.starts_with(dir));
        (!too_broad).then(|| dir.to_path_buf())
    })
}

/// Names of the project assets present at `root`.
pub(crate) fn assets_at(root: &Path) -> Vec<&'static str> {
    ASSETS
        .iter()
        .filter(|(_, relative)| std::fs::symlink_metadata(root.join(relative)).is_ok())
        .map(|(label, _)| *label)
        .collect()
}

/// The startup notice for a launch below a repository root that was not
/// moved to it: which assets there are not loaded, and how to load them.
pub(crate) fn skipped_assets_notice(launch: &Path, home: Option<&Path>) -> Option<String> {
    let root = enclosing_git_root(launch, home)?;
    let assets = assets_at(&root);
    if assets.is_empty() {
        return None;
    }
    Some(format!(
        "project assets at the repository root {} were not loaded because the session started in {}: {}. \
Start from the repository root, or set project_root_discovery = true in your user config to use the root.",
        root.display(),
        launch.display(),
        assets.join(", ")
    ))
}

/// Whether `project_root_discovery` is set in the user config file or in the
/// `--config` overrides. A project file cannot enable it: the workspace has
/// to be known before any project file is read.
pub(crate) fn discovery_enabled(user_config: Option<&Path>, overrides: &[toml::Value]) -> bool {
    let mut enabled = false;
    if let Some(path) = user_config
        && let Ok(file) = std::fs::File::open(path)
    {
        use std::io::Read as _;
        let mut text = String::new();
        // Bounded: a config larger than this is read in full by the real loader.
        if file.take(1 << 20).read_to_string(&mut text).is_ok()
            && let Ok(cfg) = crate::config::parse_config_content(path, &text)
        {
            enabled = cfg.project_root_discovery.unwrap_or(false);
        }
    }
    for value in overrides {
        if let Some(flag) = value
            .get("project_root_discovery")
            .or_else(|| value.get("project-root-discovery"))
            .and_then(toml::Value::as_bool)
        {
            enabled = flag;
        }
    }
    enabled
}

/// Moves the process to the repository root when `project_root_discovery` is
/// enabled and the launch directory is below one; returns the root. The caller
/// treats the returned directory as the workspace from here on.
pub(crate) fn apply(
    launch: &Path,
    user_config: Option<&Path>,
    overrides: &[toml::Value],
) -> std::io::Result<Option<PathBuf>> {
    let home = crate::paths::process_home_dir();
    if !discovery_enabled(user_config, overrides) {
        return Ok(None);
    }
    let Some(root) = enclosing_git_root(launch, home.as_deref()) else {
        return Ok(None);
    };
    std::env::set_current_dir(&root)?;
    Ok(Some(root))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> (crate::tests::OwnedTestDirectory, PathBuf, PathBuf) {
        let dir = crate::tests::OwnedTestDirectory::new("projroot");
        let root = dir.path().join("repo");
        let sub = root.join("packages/app");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(root.join(".mcp.json"), "{}").unwrap();
        std::fs::write(root.join("AGENTS.md"), "x").unwrap();
        std::fs::create_dir_all(root.join(".claude/skills")).unwrap();
        (dir, root, sub)
    }

    #[test]
    fn the_nearest_git_ancestor_is_found_only_below_the_root() {
        let (_dir, root, sub) = repo();
        let canonical = root.canonicalize().unwrap();
        assert_eq!(enclosing_git_root(&sub, None), Some(canonical));
        assert_eq!(enclosing_git_root(&root, None), None, "already the root");
        let nested_repo = sub.join("vendor");
        std::fs::create_dir_all(nested_repo.join(".git")).unwrap();
        assert_eq!(enclosing_git_root(&nested_repo, None), None);
        assert_eq!(
            enclosing_git_root(&nested_repo.join(".."), None),
            Some(root.canonicalize().unwrap()),
            "`..` is resolved before looking"
        );
    }

    #[test]
    fn a_home_directory_or_its_parent_repository_is_never_a_project_root() {
        let (_dir, root, sub) = repo();
        // A dotfiles-style repository in the home directory must not swallow every
        // project below it, nor a repository above the home directory.
        assert_eq!(enclosing_git_root(&sub, Some(&root)), None);
        assert_eq!(enclosing_git_root(&sub, Some(&sub)), None);
        assert!(enclosing_git_root(&sub, Some(Path::new("/nonexistent-home"))).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_dot_git_is_not_a_repository() {
        let dir = crate::tests::OwnedTestDirectory::new("projrootlink");
        let root = dir.path().join("repo");
        let sub = root.join("a");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(dir.path().join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("elsewhere"), root.join(".git")).unwrap();
        assert_eq!(enclosing_git_root(&sub, None), None);
    }

    #[test]
    fn skipped_assets_are_named_and_a_bare_repository_is_quiet() {
        let (_dir, root, sub) = repo();
        let notice = skipped_assets_notice(&sub, None).unwrap();
        for name in [
            ".mcp.json",
            "AGENTS.md",
            ".claude/skills",
            "project_root_discovery",
        ] {
            assert!(notice.contains(name), "{name}: {notice}");
        }
        assert!(!notice.contains("CLAUDE.md"));
        assert!(skipped_assets_notice(&root, None).is_none());
        std::fs::remove_file(root.join(".mcp.json")).unwrap();
        std::fs::remove_file(root.join("AGENTS.md")).unwrap();
        std::fs::remove_dir(root.join(".claude/skills")).unwrap();
        assert!(skipped_assets_notice(&sub, None).is_none());
    }

    #[test]
    fn discovery_is_default_closed_and_only_user_config_or_cli_can_open_it() {
        let dir = crate::tests::OwnedTestDirectory::new("projrootcfg");
        let user = dir.path().join("config.toml");
        assert!(!discovery_enabled(None, &[]));
        assert!(!discovery_enabled(Some(&user), &[]), "missing file");
        std::fs::write(&user, "project_root_discovery = false\n").unwrap();
        assert!(!discovery_enabled(Some(&user), &[]));
        std::fs::write(&user, "project_root_discovery = true\n").unwrap();
        assert!(discovery_enabled(Some(&user), &[]));
        let off: toml::Value = toml::from_str("project_root_discovery = false").unwrap();
        assert!(!discovery_enabled(Some(&user), &[off]), "--config wins");
        std::fs::remove_file(&user).unwrap();
        let on: toml::Value = toml::from_str("project_root_discovery = true").unwrap();
        assert!(discovery_enabled(None, &[on]));
        std::fs::write(&user, "this is not toml = = =").unwrap();
        assert!(
            !discovery_enabled(Some(&user), &[]),
            "unparseable stays closed"
        );
    }
}
