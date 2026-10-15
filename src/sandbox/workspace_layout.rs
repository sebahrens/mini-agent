//! General bubblewrap workspace destinations and captured spelling aliases.
//! This does not participate in the broker-only JavaScript worker launcher.

use std::path::{Path, PathBuf};

pub(super) const RUNTIME_RO_ROOTS: [&str; 7] =
    ["/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/nix"];

/// These paths have special namespace/control semantics. Ordinary bind or
/// alias setup failures must never silently choose a different destination.
pub(super) fn forbidden_destination(path: &Path) -> bool {
    path == Path::new("/")
        || ["/proc", "/dev", "/sys"]
            .iter()
            .any(|root| path.starts_with(root))
        || path.starts_with("/run/mini-agent")
        || Path::new("/run/mini-agent").starts_with(path)
}

pub(super) fn destination(root: &Path, bound: bool) -> PathBuf {
    if bound && forbidden_destination(root) {
        PathBuf::from("/workspace")
    } else {
        root.to_path_buf()
    }
}

pub(super) struct WorkspaceLayout {
    pub(super) destination: PathBuf,
    pub(super) runtime_ro: Vec<(PathBuf, PathBuf)>,
    pub(super) temporary_root: PathBuf,
    /// Empty namespace directories, created before ANY writable host bind.
    pub(super) alias_directories: Vec<PathBuf>,
    pub(super) aliases: Vec<(PathBuf, PathBuf)>,
}

impl WorkspaceLayout {
    pub(super) fn new(
        workspace: Option<&crate::paths::WorkspaceBinding>,
        source: &Path,
    ) -> Result<Self, String> {
        let root = workspace.map_or(source, crate::paths::WorkspaceBinding::root);
        let mut value = Self {
            destination: destination(root, workspace.is_some()),
            runtime_ro: RUNTIME_RO_ROOTS
                .iter()
                .map(|root| (PathBuf::from(root), PathBuf::from(root)))
                .collect(),
            temporary_root: PathBuf::from("/tmp"),
            alias_directories: Vec::new(),
            aliases: Vec::new(),
        };
        #[cfg(target_os = "linux")]
        if let Some(workspace) = workspace
            && !forbidden_destination(root)
        {
            let links = workspace.spelling_links();
            for link in links {
                if forbidden_destination(&link.path) || forbidden_destination(&link.target) {
                    return Err(
                        "workspace spelling overlaps a sandbox namespace or control path".into(),
                    );
                }
                for (source, destination) in &mut value.runtime_ro {
                    if source == &link.path {
                        // Keep exactly the same readonly source, but avoid a
                        // duplicate readonly view that hides the workspace
                        // overlay through /bin, /lib, etc.
                        *destination = link.target.clone();
                    }
                }
                if link.path == Path::new("/tmp") {
                    value.temporary_root = link.target.clone();
                }
                if !value.alias_directories.contains(&link.target) {
                    value.alias_directories.push(link.target.clone());
                }
            }
            for link in links {
                let exposed_readonly = value
                    .runtime_ro
                    .iter()
                    .any(|(_, destination)| link.path.starts_with(destination));
                let target = if exposed_readonly {
                    // Existing readonly links cannot be unlinked/recreated.
                    // Bubblewrap's idempotent check must compare exact bytes.
                    link.literal_target.clone()
                } else {
                    link.target.clone()
                };
                value.aliases.push((link.path.clone(), target));
            }
            value
                .aliases
                .sort_by_key(|(path, _)| path.components().count());
        }
        #[cfg(not(target_os = "linux"))]
        let _ = &mut value;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_workspace_destinations_have_exact_forbidden_boundaries() {
        for path in [
            "/",
            "/proc",
            "/proc/project",
            "/dev/shm/project",
            "/sys/project",
            "/run",
            "/run/mini-agent",
            "/run/mini-agent/image",
        ] {
            assert_eq!(destination(Path::new(path), true), Path::new("/workspace"));
        }
        for path in [
            "/tmp",
            "/tmp/repo",
            "/usr",
            "/usr/repo",
            "/proc-project",
            "/dev-project",
            "/sys-project",
            "/run/mini-agent-other",
            "/run/user/repo",
            "/workspace",
            "/workspace/repo",
        ] {
            assert_eq!(destination(Path::new(path), true), Path::new(path));
        }
        assert_eq!(
            destination(Path::new("/dev/shm/repo"), false),
            Path::new("/dev/shm/repo")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_workspace_layout_captures_fresh_aliases_and_rehomes_runtime_aliases() {
        use std::os::unix::fs::symlink;
        let owned = crate::tests::OwnedTestDirectory::new("workspace-layout");
        let real = owned.path().join("real");
        std::fs::create_dir_all(real.join("repo")).unwrap();
        let spelled = owned.path().join("spelled");
        symlink("real", &spelled).unwrap();
        let binding = crate::paths::WorkspaceBinding::capture(&spelled.join("repo")).unwrap();
        let layout = WorkspaceLayout::new(Some(&binding), Path::new("/proc/self/fd/197")).unwrap();
        assert_eq!(layout.destination, real.join("repo"));
        assert_eq!(layout.aliases, vec![(spelled, real)]);

        // A merged-/usr Linux installation supplies a genuine runtime alias.
        if std::fs::symlink_metadata("/bin")
            .unwrap()
            .file_type()
            .is_symlink()
        {
            let binding = crate::paths::WorkspaceBinding::capture(Path::new("/bin")).unwrap();
            let layout =
                WorkspaceLayout::new(Some(&binding), Path::new("/proc/self/fd/197")).unwrap();
            let canonical = std::fs::canonicalize("/bin").unwrap();
            assert!(
                layout
                    .runtime_ro
                    .iter()
                    .any(|(source, target)| source == Path::new("/bin") && target == &canonical)
            );
            assert!(
                layout
                    .aliases
                    .iter()
                    .any(|(path, target)| path == Path::new("/bin") && target == &canonical)
            );
            assert_eq!(layout.destination, canonical);
        }
    }
}
