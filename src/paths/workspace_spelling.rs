//! Captured Linux workspace spellings for the general-process mount layout.
//!
//! These observations are metadata only. They do not grant ambient parent
//! reads to file tools and never replace the retained canonical root authority.

use std::collections::HashSet;
use std::io;
use std::path::{Component, Path, PathBuf};

const MAX_LINKS: usize = 64;

#[derive(Debug, Clone)]
pub(super) struct WorkspaceSpelling {
    path: PathBuf,
    links: Vec<WorkspaceLink>,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceLink {
    pub(crate) path: PathBuf,
    pub(crate) target: PathBuf,
    pub(crate) literal_target: PathBuf,
    identity: crate::fs::CheckedMetadata,
}

impl WorkspaceSpelling {
    pub(super) fn capture(path: &Path, root: &Path) -> io::Result<Self> {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let mut value = Self {
            path,
            links: Vec::new(),
        };
        let mut seen = HashSet::new();
        let resolved = value.resolve(&value.path.clone(), &mut seen)?;
        if resolved != root {
            return Err(changed());
        }
        value.validate()?;
        Ok(value)
    }

    fn resolve(&mut self, path: &Path, seen: &mut HashSet<PathBuf>) -> io::Result<PathBuf> {
        let mut cursor = PathBuf::from("/");
        for component in path.components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::ParentDir => {
                    cursor.pop();
                    if cursor.as_os_str().is_empty() {
                        cursor.push("/");
                    }
                }
                Component::Normal(name) => {
                    let next = cursor.join(name);
                    let shape = std::fs::symlink_metadata(&next)?;
                    if !shape.file_type().is_symlink() {
                        cursor = next;
                        continue;
                    }
                    let target = std::fs::canonicalize(&next)?;
                    if !seen.insert(next.clone()) {
                        cursor = target;
                        continue;
                    }
                    if self.links.len() == MAX_LINKS {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "workspace spelling has too many symbolic links",
                        ));
                    }
                    // Linux checked_path_metadata uses O_PATH|O_NOFOLLOW:
                    // a replacement FIFO/device is never opened for reading.
                    let identity = crate::fs::checked_path_metadata(&next)?;
                    if !identity.file_type().is_symlink() {
                        return Err(changed());
                    }
                    let literal_target = std::fs::read_link(&next)?;
                    let followed = if literal_target.is_absolute() {
                        literal_target.clone()
                    } else {
                        cursor.join(&literal_target)
                    };
                    self.links.push(WorkspaceLink {
                        path: next,
                        target: target.clone(),
                        literal_target,
                        identity,
                    });
                    // Capture indirect links too: an already exposed readonly
                    // symlink retains its literal target inside the namespace.
                    if self.resolve(&followed, seen)? != target {
                        return Err(changed());
                    }
                    cursor = target;
                }
                Component::Prefix(_) => return Err(changed()),
            }
        }
        Ok(cursor)
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn links(&self) -> &[WorkspaceLink] {
        &self.links
    }

    pub(super) fn validate(&self) -> io::Result<()> {
        for link in &self.links {
            let before = crate::fs::checked_path_metadata(&link.path)?;
            crate::fs::ensure_same_file(&link.path, &link.identity, &before)?;
            if !before.file_type().is_symlink()
                || std::fs::read_link(&link.path)? != link.literal_target
            {
                return Err(changed());
            }
            let after = crate::fs::checked_path_metadata(&link.path)?;
            crate::fs::ensure_same_file(&link.path, &link.identity, &after)?;
        }
        Ok(())
    }
}

fn changed() -> io::Error {
    io::Error::other("workspace spelling changed during capture or after session creation")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn linux_workspace_spelling_captures_indirect_links_without_parent_content_authority() {
        let owned = crate::tests::OwnedTestDirectory::new("workspace-spelling");
        let real = owned.path().join("real");
        std::fs::create_dir_all(real.join("repo")).unwrap();
        symlink("real", owned.path().join("indirect")).unwrap();
        symlink("indirect", owned.path().join("spelled")).unwrap();
        let spelling = owned.path().join("spelled/repo");
        let binding = crate::paths::WorkspaceBinding::capture(&spelling).unwrap();
        assert_eq!(binding.root(), real.join("repo"));
        assert_eq!(binding.spelled_root(), spelling);
        assert_eq!(binding.spelling_links().len(), 2);
        assert_eq!(
            binding.spelling_links()[0].literal_target,
            Path::new("indirect")
        );
        assert_eq!(
            binding.spelling_links()[1].literal_target,
            Path::new("real")
        );
        binding.validate().unwrap();
        assert!(binding.open_relative(Path::new("../outside")).is_err());
    }

    #[test]
    fn linux_workspace_spelling_replacement_refuses_even_an_equal_target() {
        let owned = crate::tests::OwnedTestDirectory::new("workspace-spelling-replace");
        std::fs::create_dir(owned.path().join("real")).unwrap();
        let spelled = owned.path().join("spelled");
        symlink("real", &spelled).unwrap();
        let binding = crate::paths::WorkspaceBinding::capture(&spelled).unwrap();
        let clone = binding.try_clone().unwrap();
        assert_eq!(clone.spelled_root(), spelled);
        clone.validate().unwrap();
        std::fs::remove_file(&spelled).unwrap();
        symlink("real", &spelled).unwrap();
        assert!(binding.validate().is_err());
        assert!(clone.validate().is_err());
        assert_eq!(binding.root(), owned.path().join("real"));
    }

    #[test]
    #[allow(unsafe_code)]
    fn linux_workspace_spelling_replaced_fifo_is_rejected_without_opening_it() {
        use std::os::unix::ffi::OsStrExt;
        let owned = crate::tests::OwnedTestDirectory::new("workspace-spelling-fifo");
        std::fs::create_dir(owned.path().join("real")).unwrap();
        let spelled = owned.path().join("spelled");
        symlink("real", &spelled).unwrap();
        let binding = crate::paths::WorkspaceBinding::capture(&spelled).unwrap();
        std::fs::remove_file(&spelled).unwrap();
        let name = std::ffi::CString::new(spelled.as_os_str().as_bytes()).unwrap();
        // No writer ever opens this FIFO. O_PATH metadata must return directly.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(binding.validate().is_err());
    }
}
