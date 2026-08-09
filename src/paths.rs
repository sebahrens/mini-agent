use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::OnceLock;

/// A canonical workspace directory plus the filesystem identity captured when
/// an ACP session was created.  Keeping the pathname alone is insufficient:
/// an attacker could rename the directory and replace it with a symlink (or a
/// different directory) while a long-lived session is still active.
#[derive(Debug)]
pub(crate) struct WorkspaceBinding {
    root: PathBuf,
    directory: std::fs::File,
    capability: cap_std::fs::Dir,
    ancestors: Vec<(PathBuf, cap_std::fs::Dir)>,
    identity: crate::fs::CheckedMetadata,
}

impl WorkspaceBinding {
    pub(crate) fn capture(path: &Path) -> io::Result<Self> {
        let root = std::fs::canonicalize(path)?;
        let capability = cap_std::fs::Dir::open_ambient_dir(&root, cap_std::ambient_authority())?;
        #[cfg(not(windows))]
        let directory = std::fs::File::open(&root)?;
        #[cfg(windows)]
        let directory = {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE,
            };
            std::fs::OpenOptions::new()
                .read(true)
                // Excluding FILE_SHARE_DELETE pins the directory name while
                // the ACP session's binding is alive.
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open(&root)?
        };
        let metadata = crate::fs::checked_file_metadata(&directory)?;
        let capability_file = capability.try_clone()?.into_std_file();
        let capability_metadata = crate::fs::checked_file_metadata(&capability_file)?;
        crate::fs::ensure_same_file(&root, &metadata, &capability_metadata)?;
        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workspace path is not a directory",
            ));
        }
        // ACP sessions deliberately omit ambient parent context. Retaining
        // only the captured workspace capability prevents a coherently
        // reparented ancestor chain from injecting instructions after capture.
        let ancestors = vec![(root.clone(), capability.try_clone()?)];
        Ok(Self {
            root,
            directory,
            capability,
            ancestors,
            identity: metadata,
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn logical_relative_path(&self, path: &Path) -> io::Result<PathBuf> {
        self.normalize_relative(path)
            .map(|relative| self.root.join(relative))
    }

    pub(crate) fn open_relative(&self, path: &Path) -> io::Result<std::fs::File> {
        let path = self.normalize_relative(path)?;
        let (parent, name) = self.open_parent(&path, false)?;
        open_file_no_follow(&parent, &name)
    }

    pub(crate) fn open_dir_relative(&self, path: &Path) -> io::Result<cap_std::fs::Dir> {
        let path = self.normalize_relative(path)?;
        self.open_directory_path(&path, false)
    }

    pub(crate) fn open_relative_directory_file(&self, path: &Path) -> io::Result<std::fs::File> {
        self.open_dir_relative(path)
            .map(cap_std::fs::Dir::into_std_file)
    }

    pub(crate) fn read_relative_dir_files(
        &self,
        path: &Path,
        extension: &str,
    ) -> io::Result<Vec<(String, String)>> {
        let directory = self.open_dir_relative(path)?;
        let mut files = Vec::new();
        for entry in directory.read_dir(".")? {
            let entry = entry?;
            let name = entry.file_name();
            let candidate = Path::new(&name);
            if candidate
                .extension()
                .is_some_and(|value| value == extension)
                && let Some(stem) = candidate.file_stem().and_then(|value| value.to_str())
                && let Ok(mut file) = open_file_no_follow(&directory, &name)
            {
                let mut content = String::new();
                if file.read_to_string(&mut content).is_err() {
                    continue;
                }
                files.push((stem.to_string(), content));
            }
        }
        Ok(files)
    }

    pub(crate) fn read_ancestor_files(&self, names: &[&str]) -> Vec<(PathBuf, String, String)> {
        let mut files = Vec::new();
        for (path, directory) in &self.ancestors {
            for name in names {
                if let Ok(mut file) = open_file_no_follow(directory, OsStr::new(name)) {
                    let mut content = String::new();
                    if file.read_to_string(&mut content).is_err() {
                        continue;
                    }
                    files.push((path.clone(), (*name).to_string(), content));
                }
            }
        }
        files
    }

    pub(crate) fn create_relative_atomic(&self, path: &Path, content: &[u8]) -> io::Result<()> {
        let path = self.normalize_relative(path)?;
        let (parent, name) = self.open_parent(&path, true)?;
        let temp = sibling_temp_name(&name);
        let result = (|| {
            let mut options = cap_std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use cap_std::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = parent.open_with(&temp, &options)?;
            file.write_all(content)?;
            file.flush()?;
            parent.hard_link(&temp, &parent, &name)?;
            Ok(())
        })();
        let _ = parent.remove_file(&temp);
        result
    }

    pub(crate) fn replace_relative_atomic(
        &self,
        path: &Path,
        content: &[u8],
        expected: &crate::fs::CheckedMetadata,
    ) -> io::Result<()> {
        self.replace_relative_atomic_with_hook(path, content, expected, || {})
    }

    fn replace_relative_atomic_with_hook(
        &self,
        path: &Path,
        content: &[u8],
        expected: &crate::fs::CheckedMetadata,
        before_final_identity_check: impl FnOnce(),
    ) -> io::Result<()> {
        if !expected.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workspace replacement target is not a regular file",
            ));
        }
        let path = self.normalize_relative(path)?;
        let (parent, name) = self.open_parent(&path, false)?;
        let current = open_file_no_follow(&parent, &name)?;
        let current_metadata = crate::fs::checked_file_metadata(&current)?;
        if !current_metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workspace replacement target is not a regular file",
            ));
        }
        crate::fs::ensure_same_file(&path, expected, &current_metadata)?;
        let temp = sibling_temp_name(&name);
        let result = (|| {
            let mut options = cap_std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use cap_std::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = parent.open_with(&temp, &options)?;
            file.write_all(content)?;
            file.flush()?;
            let file = file.into_std();
            file.set_permissions(expected.permissions())?;
            let replacement_metadata = crate::fs::checked_file_metadata(&file)?;
            drop(file);
            let current = open_file_no_follow(&parent, &name)?;
            let current_metadata = crate::fs::checked_file_metadata(&current)?;
            if !current_metadata.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "workspace replacement target is not a regular file",
                ));
            }
            crate::fs::ensure_same_file(&path, expected, &current_metadata)?;
            drop(current);
            before_final_identity_check();
            exchange_relative(&parent, &temp, &name)?;

            if entry_matches_regular_file(&parent, &temp, expected)? {
                parent.remove_file(&temp)?;
                return Ok(());
            }

            restore_after_failed_exchange(&parent, &temp, &name, &replacement_metadata)?;
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("Path changed after permission check: {}", path.display()),
            ))
        })();
        let _ = parent.remove_file(&temp);
        result
    }

    fn open_parent(
        &self,
        path: &Path,
        create_missing: bool,
    ) -> io::Result<(cap_std::fs::Dir, OsString)> {
        let name = path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "workspace target has no file name",
            )
        })?;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        Ok((
            self.open_directory_path(parent, create_missing)?,
            name.to_os_string(),
        ))
    }

    fn open_directory_path(
        &self,
        path: &Path,
        create_missing: bool,
    ) -> io::Result<cap_std::fs::Dir> {
        // Open `.` relative to the retained capability to get an independent
        // file description. A plain descriptor clone shares its directory
        // cursor on Unix, so one search could otherwise consume another.
        let mut directory = open_dir_no_follow(&self.capability, OsStr::new("."))?;
        for component in path.components() {
            let std::path::Component::Normal(name) = component else {
                if matches!(component, std::path::Component::CurDir) {
                    continue;
                }
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "workspace capability requires a contained relative path",
                ));
            };
            directory = match open_dir_no_follow(&directory, name) {
                Ok(child) => child,
                Err(error) if create_missing && error.kind() == io::ErrorKind::NotFound => {
                    match directory.create_dir(name) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(error),
                    }
                    open_dir_no_follow(&directory, name)?
                }
                Err(error) => return Err(error),
            };
        }
        Ok(directory)
    }

    fn normalize_relative(&self, path: &Path) -> io::Result<PathBuf> {
        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::CurDir => {}
                std::path::Component::Normal(part) => normalized.push(part),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "workspace capability requires a contained relative path",
                    ));
                }
            }
        }
        if normalized.as_os_str().is_empty() {
            normalized.push(".");
        }
        Ok(normalized)
    }

    pub(crate) fn try_clone_directory_file(&self) -> io::Result<std::fs::File> {
        self.directory.try_clone()
    }

    /// Fail closed if the canonical target or its directory identity changed.
    /// Consumers call this immediately before each filesystem/process effect.
    pub(crate) fn validate(&self) -> Result<(), String> {
        let canonical = std::fs::canonicalize(&self.root)
            .map_err(|error| format!("workspace binding is unavailable: {error}"))?;
        if canonical != self.root {
            return Err("workspace binding changed after session creation".to_string());
        }
        let metadata = crate::fs::checked_path_metadata(&canonical)
            .map_err(|error| format!("workspace binding is unavailable: {error}"))?;
        if !metadata.is_dir() {
            return Err("workspace binding is no longer a directory".to_string());
        }
        crate::fs::ensure_same_file(&canonical, &self.identity, &metadata)
            .map_err(|_| "workspace binding identity changed after session creation".to_string())?;
        let held = crate::fs::checked_file_metadata(&self.directory)
            .map_err(|error| format!("workspace binding handle is unavailable: {error}"))?;
        crate::fs::ensure_same_file(&canonical, &self.identity, &held)
            .map_err(|_| "workspace binding handle identity changed".to_string())?;
        Ok(())
    }
}

fn sibling_temp_name(name: &OsStr) -> OsString {
    OsString::from(format!(
        ".{}.mini-agent-{}",
        name.to_string_lossy(),
        uuid::Uuid::new_v4()
    ))
}

/// Atomically exchange two names in one retained directory. The old target is
/// left at `replacement`, allowing its identity to be validated after the
/// publication point without a check-to-rename race.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(unsafe_code)]
fn exchange_relative(
    directory: &cap_std::fs::Dir,
    replacement: &OsStr,
    target: &OsStr,
) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let replacement = CString::new(replacement.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace path component contains NUL",
        )
    })?;
    let target = CString::new(target.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace path component contains NUL",
        )
    })?;
    #[cfg(target_os = "linux")]
    // SAFETY: the names and retained directory descriptor remain valid for
    // this syscall, which confines the exchange to the held directory.
    let result = unsafe {
        crate::fs::linux_renameat2(
            directory.as_raw_fd(),
            replacement.as_ptr(),
            directory.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    #[cfg(target_os = "macos")]
    // SAFETY: the names and retained directory descriptor remain valid for
    // this syscall, which confines the swap to the held directory.
    let result = unsafe {
        libc::renameatx_np(
            directory.as_raw_fd(),
            replacement.as_ptr(),
            directory.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_SWAP,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(unsafe_code)]
fn rename_no_replace_relative(
    directory: &cap_std::fs::Dir,
    source: &OsStr,
    target: &OsStr,
) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path component contains NUL"))?;
    let target = CString::new(target.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path component contains NUL"))?;
    #[cfg(target_os = "linux")]
    // SAFETY: the names and retained directory descriptor remain valid for
    // this syscall, which confines the rename to the held directory.
    let result = unsafe {
        crate::fs::linux_renameat2(
            directory.as_raw_fd(),
            source.as_ptr(),
            directory.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    // SAFETY: the names and retained directory descriptor remain valid for
    // this syscall, which confines the rename to the held directory.
    let result = unsafe {
        libc::renameatx_np(
            directory.as_raw_fd(),
            source.as_ptr(),
            directory.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn exchange_relative(
    _directory: &cap_std::fs::Dir,
    _replacement: &OsStr,
    _target: &OsStr,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic compare-and-replace is unsupported on this platform",
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rename_no_replace_relative(
    _directory: &cap_std::fs::Dir,
    _source: &OsStr,
    _target: &OsStr,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic compare-and-replace is unsupported on this platform",
    ))
}

/// Restore the newest raced target after an exchange displaced an inode other
/// than the caller's expected one. Publication is create-if-absent, so a
/// still-newer writer is never overwritten during rollback.
fn restore_after_failed_exchange(
    directory: &cap_std::fs::Dir,
    displaced: &OsStr,
    target: &OsStr,
    replacement_metadata: &crate::fs::CheckedMetadata,
) -> io::Result<()> {
    let rollback = sibling_temp_name(target);
    if let Err(error) = rename_no_replace_relative(directory, target, &rollback) {
        if error.kind() == io::ErrorKind::NotFound {
            // A concurrent delete after the exchange is the newest state.
            return Ok(());
        }
        return Err(error);
    }

    let rollback_is_replacement =
        entry_matches_regular_file(directory, &rollback, replacement_metadata)?;
    let restore_source = if rollback_is_replacement {
        displaced
    } else {
        rollback.as_os_str()
    };

    match rename_no_replace_relative(directory, restore_source, target) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            // A later writer won while rollback was in progress.
        }
        Err(error) => return Err(error),
    }
    if rollback_is_replacement {
        let _ = directory.remove_file(&rollback);
    }
    Ok(())
}

fn entry_matches_regular_file(
    directory: &cap_std::fs::Dir,
    name: &OsStr,
    expected: &crate::fs::CheckedMetadata,
) -> io::Result<bool> {
    let entry = directory.symlink_metadata(name)?;
    if !entry.is_file() {
        return Ok(false);
    }
    let file = open_file_no_follow(directory, name)?;
    let current = crate::fs::checked_file_metadata(&file)?;
    Ok(crate::fs::ensure_same_file(Path::new("workspace replacement"), expected, &current).is_ok())
}

fn open_file_no_follow(directory: &cap_std::fs::Dir, name: &OsStr) -> io::Result<std::fs::File> {
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(not(any(unix, windows)))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symlink-safe workspace capabilities are unsupported on this platform",
    ));

    let file = directory.open_with(name, &options)?.into_std();
    #[cfg(windows)]
    reject_windows_reparse_point(&file)?;
    Ok(file)
}

fn open_dir_no_follow(directory: &cap_std::fs::Dir, name: &OsStr) -> io::Result<cap_std::fs::Dir> {
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
            FILE_SHARE_WRITE,
        };
        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(not(any(unix, windows)))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symlink-safe workspace capabilities are unsupported on this platform",
    ));

    let file = directory.open_with(name, &options)?.into_std();
    #[cfg(windows)]
    reject_windows_reparse_point(&file)?;
    if !file.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "workspace path component is not a directory",
        ));
    }
    Ok(cap_std::fs::Dir::from_std_file(file))
}

#[cfg(windows)]
fn reject_windows_reparse_point(file: &std::fs::File) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "workspace path traverses a reparse point",
        ));
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod workspace_binding_tests {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::WorkspaceBinding;

    fn temp_root(label: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "mini-agent-workspace-binding-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn ancestor_context_is_limited_to_the_captured_workspace_root() {
        let container = temp_root("root-context").canonicalize().unwrap();
        let root = container.join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(container.join("AGENTS.md"), "parent injection").unwrap();
        std::fs::write(root.join("AGENTS.md"), "workspace instruction").unwrap();

        let binding = WorkspaceBinding::capture(&root).unwrap();
        let context = binding.read_ancestor_files(&["AGENTS.md"]);
        assert_eq!(
            context,
            vec![(
                root.clone(),
                "AGENTS.md".to_string(),
                "workspace instruction".to_string()
            )]
        );

        std::fs::remove_dir_all(container).unwrap();
    }

    #[test]
    fn capability_operations_reject_symlink_components_and_targets() {
        let root = temp_root("symlink-traversal");
        std::fs::create_dir_all(root.join("safe")).unwrap();
        std::fs::create_dir_all(root.join("secret")).unwrap();
        std::fs::write(root.join("secret/value.txt"), "secret").unwrap();
        symlink("../secret/value.txt", root.join("safe/link.txt")).unwrap();
        symlink("../secret", root.join("safe/link-dir")).unwrap();
        let binding = WorkspaceBinding::capture(&root).unwrap();

        assert!(binding.open_relative(Path::new("safe/link.txt")).is_err());
        assert!(
            binding
                .create_relative_atomic(Path::new("safe/link-dir/new.txt"), b"bad")
                .is_err()
        );
        assert!(!root.join("secret/new.txt").exists());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn capability_atomic_writes_create_private_and_preserve_existing_mode() {
        let root = temp_root("atomic-mode");
        let binding = WorkspaceBinding::capture(&root).unwrap();

        binding
            .create_relative_atomic(std::path::Path::new("created.txt"), b"created")
            .unwrap();
        let created_mode = std::fs::metadata(root.join("created.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(created_mode, 0o600);

        let existing = root.join("existing.sh");
        std::fs::write(&existing, "old").unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o751)).unwrap();
        let expected_file = binding
            .open_relative(std::path::Path::new("existing.sh"))
            .unwrap();
        let expected = crate::fs::checked_file_metadata(&expected_file).unwrap();
        binding
            .replace_relative_atomic(std::path::Path::new("existing.sh"), b"new", &expected)
            .unwrap();
        let replaced = std::fs::metadata(&existing).unwrap();
        assert_eq!(replaced.permissions().mode() & 0o777, 0o751);
        assert_eq!(std::fs::read_to_string(existing).unwrap(), "new");

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn capability_atomic_replace_detects_a_race_at_the_atomic_swap() {
        let root = temp_root("atomic-cas");
        let binding = WorkspaceBinding::capture(&root).unwrap();
        std::fs::write(root.join("target.txt"), "expected").unwrap();
        std::fs::write(root.join("concurrent.txt"), "concurrent").unwrap();
        let expected_file = binding.open_relative(Path::new("target.txt")).unwrap();
        let expected = crate::fs::checked_file_metadata(&expected_file).unwrap();

        let result = binding.replace_relative_atomic_with_hook(
            Path::new("target.txt"),
            b"agent replacement",
            &expected,
            || std::fs::rename(root.join("concurrent.txt"), root.join("target.txt")).unwrap(),
        );

        assert!(result.is_err(), "concurrent replacement must fail the CAS");
        assert_eq!(
            std::fs::read_to_string(root.join("target.txt")).unwrap(),
            "concurrent"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn capability_atomic_replace_rejects_an_existing_directory_without_mutation() {
        let root = temp_root("atomic-directory");
        let binding = WorkspaceBinding::capture(&root).unwrap();
        std::fs::create_dir(root.join("target")).unwrap();
        std::fs::write(root.join("target/sentinel.txt"), "directory-content").unwrap();
        let expected = crate::fs::checked_path_metadata(&root.join("target")).unwrap();

        let result =
            binding.replace_relative_atomic(Path::new("target"), b"agent replacement", &expected);

        assert!(result.is_err());
        assert!(root.join("target").is_dir());
        assert_eq!(
            std::fs::read_to_string(root.join("target/sentinel.txt")).unwrap(),
            "directory-content"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn capability_atomic_replace_restores_raced_directory_and_symlink_targets() {
        let root = temp_root("atomic-raced-types");
        let binding = WorkspaceBinding::capture(&root).unwrap();

        std::fs::write(root.join("directory-target"), "expected").unwrap();
        std::fs::create_dir(root.join("raced-directory")).unwrap();
        std::fs::write(root.join("raced-directory/sentinel.txt"), "raced-directory").unwrap();
        let expected = crate::fs::checked_path_metadata(&root.join("directory-target")).unwrap();
        let result = binding.replace_relative_atomic_with_hook(
            Path::new("directory-target"),
            b"agent replacement",
            &expected,
            || {
                std::fs::rename(
                    root.join("directory-target"),
                    root.join("parked-directory-target"),
                )
                .unwrap();
                std::fs::rename(root.join("raced-directory"), root.join("directory-target"))
                    .unwrap();
            },
        );
        assert!(result.is_err());
        assert!(root.join("directory-target").is_dir());
        assert_eq!(
            std::fs::read_to_string(root.join("directory-target/sentinel.txt")).unwrap(),
            "raced-directory"
        );

        std::fs::write(root.join("symlink-target"), "expected").unwrap();
        std::fs::write(root.join("symlink-referent"), "referent").unwrap();
        let expected = crate::fs::checked_path_metadata(&root.join("symlink-target")).unwrap();
        let result = binding.replace_relative_atomic_with_hook(
            Path::new("symlink-target"),
            b"agent replacement",
            &expected,
            || {
                std::fs::rename(
                    root.join("symlink-target"),
                    root.join("parked-symlink-target"),
                )
                .unwrap();
                symlink("symlink-referent", root.join("symlink-target")).unwrap();
            },
        );
        assert!(result.is_err());
        assert!(
            std::fs::symlink_metadata(root.join("symlink-target"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(root.join("symlink-target")).unwrap(),
            "referent"
        );

        std::fs::remove_dir_all(root).unwrap();
    }
}

// These shared path-policy primitives are exercised by the acceptance suite
// before all of their production consumers land.
#[allow(dead_code)]
pub mod portable;

#[allow(unused_imports)]
pub use portable::{
    MAX_PORTABLE_COMPONENT_BYTES, MAX_PORTABLE_COMPONENT_UTF16_UNITS, MAX_PORTABLE_PATH_BYTES,
    MAX_PORTABLE_PATH_UTF16_UNITS, PortablePathError, collision_key, contained_join,
    ensure_contained, ensure_no_link_traversal, validate_portable_relative_path,
};
pub use portable::{digest_filename, opaque_name, validate_portable_component};

const APP_COMPONENT: &str = crate::product::LEGACY_APP_COMPONENT;
const MIGRATION_VERSION: u32 = 1;
pub(crate) const PRIVATE_PATH_LINK_POLICY: &str = "reject symlinked path components";

static PROCESS_PATHS: OnceLock<AppPaths> = OnceLock::new();
static DISABLED_ARTIFACTS: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathPlatform {
    Linux,
    MacOs,
    Windows,
}

impl PathPlatform {
    fn current() -> Result<Self, AppPathError> {
        if cfg!(target_os = "linux") {
            Ok(Self::Linux)
        } else if cfg!(target_os = "macos") {
            Ok(Self::MacOs)
        } else if cfg!(target_os = "windows") {
            Ok(Self::Windows)
        } else {
            Err(AppPathError::UnsupportedPlatform)
        }
    }
}

impl std::fmt::Display for PathPlatform {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Linux => formatter.write_str("Linux"),
            Self::MacOs => formatter.write_str("macOS"),
            Self::Windows => formatter.write_str("Windows"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppPathRoot {
    Config,
    Data,
    LocalData,
    State,
    Cache,
    Workspace,
}

impl std::fmt::Display for AppPathRoot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config => formatter.write_str("configuration"),
            Self::Data => formatter.write_str("portable data"),
            Self::LocalData => formatter.write_str("local data"),
            Self::State => formatter.write_str("state"),
            Self::Cache => formatter.write_str("cache"),
            Self::Workspace => formatter.write_str("workspace"),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AppPathError {
    #[error("this operating system is not supported")]
    UnsupportedPlatform,
    #[error("required {root} base directory is unavailable on {platform}")]
    MissingBase {
        root: AppPathRoot,
        platform: PathPlatform,
    },
    #[error("{variable} is set but empty")]
    EmptyOverride { variable: &'static str },
    #[error("{variable} uses '~', but the home directory is unavailable")]
    MissingHomeForTilde { variable: &'static str },
    #[error("{variable} must be an absolute path, got {value:?}")]
    RelativeOverride {
        variable: &'static str,
        value: PathBuf,
    },
    #[error("the {root} base directory must be absolute, got {value:?}")]
    RelativeBase { root: AppPathRoot, value: PathBuf },
    #[error("application paths were already initialized with different roots")]
    AlreadyInitialized,
    #[cfg_attr(test, allow(dead_code))]
    #[error("application paths have not been initialized")]
    NotInitialized,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathOverrides {
    pub config_dir: Option<OsString>,
    pub data_dir: Option<OsString>,
    pub local_data_dir: Option<OsString>,
    pub state_dir: Option<OsString>,
    pub cache_dir: Option<OsString>,
    pub credentials_dir: Option<OsString>,
}

impl PathOverrides {
    fn from_process() -> Self {
        Self {
            config_dir: std::env::var_os("ZS_CONFIG_DIR"),
            data_dir: std::env::var_os("ZS_DATA_DIR"),
            local_data_dir: std::env::var_os("ZS_LOCAL_DATA_DIR"),
            state_dir: std::env::var_os("ZS_STATE_DIR"),
            cache_dir: std::env::var_os("ZS_CACHE_DIR"),
            credentials_dir: std::env::var_os("ZS_CREDENTIALS_DIR"),
        }
    }
}

/// All host inputs used to resolve application paths.
///
/// Production captures this value once. Tests construct it directly, avoiding
/// process-global environment mutation and host-dependent expectations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathEnvironment {
    pub platform: PathPlatform,
    pub home_dir: Option<PathBuf>,
    pub config_base: Option<PathBuf>,
    pub data_base: Option<PathBuf>,
    pub local_data_base: Option<PathBuf>,
    pub state_base: Option<PathBuf>,
    pub cache_base: Option<PathBuf>,
    pub workspace_root: Option<PathBuf>,
    pub overrides: PathOverrides,
}

impl PathEnvironment {
    pub fn from_process(workspace_root: Option<PathBuf>) -> Result<Self, AppPathError> {
        Ok(Self {
            platform: PathPlatform::current()?,
            home_dir: dirs::home_dir(),
            config_base: dirs::config_dir(),
            data_base: dirs::data_dir(),
            local_data_base: dirs::data_local_dir(),
            state_base: dirs::state_dir(),
            cache_base: dirs::cache_dir(),
            workspace_root,
            overrides: PathOverrides::from_process(),
        })
    }
}

/// Immutable, fully resolved roots for all persistent application storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub local_data_dir: PathBuf,
    pub state_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub credentials_dir: PathBuf,
    pub project_dir: Option<PathBuf>,
}

/// Typed owner for the private, machine-local JavaScript effect audit.
///
/// Callers receive only fixed child paths below the resolved state root; effect metadata can never
/// influence an audit filename or redirect the writer into the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectAuditPathOwner {
    state_root: PathBuf,
    directory: PathBuf,
}

impl EffectAuditPathOwner {
    pub fn state_root(&self) -> PathBuf {
        self.state_root.clone()
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn lock_file(&self) -> PathBuf {
        self.directory.join("writer.lock")
    }

    pub fn target_key_file(&self) -> PathBuf {
        self.directory.join("target-hmac-v1.key")
    }

    pub fn initialization_marker(&self) -> PathBuf {
        self.state_root.join("js-effect-audit-v1.initialized")
    }

    pub(crate) fn segment_file(&self, index: u64) -> PathBuf {
        self.directory.join(format!("segment-{index:020}.audit"))
    }
}

impl AppPaths {
    pub fn from_process(workspace_root: Option<PathBuf>) -> Result<Self, AppPathError> {
        if let Some(paths) = PROCESS_PATHS.get() {
            return Ok(paths.clone());
        }
        Self::resolve(&PathEnvironment::from_process(workspace_root)?)
    }

    pub fn resolve(environment: &PathEnvironment) -> Result<Self, AppPathError> {
        let platform = environment.platform;
        let config_override = resolve_override(
            environment,
            "ZS_CONFIG_DIR",
            environment.overrides.config_dir.as_deref(),
        )?;
        let data_override = resolve_override(
            environment,
            "ZS_DATA_DIR",
            environment.overrides.data_dir.as_deref(),
        )?;
        let local_data_override = resolve_override(
            environment,
            "ZS_LOCAL_DATA_DIR",
            environment.overrides.local_data_dir.as_deref(),
        )?;
        let state_override = resolve_override(
            environment,
            "ZS_STATE_DIR",
            environment.overrides.state_dir.as_deref(),
        )?;

        let config_dir = match config_override {
            Some(path) => path,
            None => default_root(environment, AppPathRoot::Config, &environment.config_base)?,
        };
        let data_dir = match &data_override {
            Some(path) => path.clone(),
            None => default_root(environment, AppPathRoot::Data, &environment.data_base)?,
        };
        let local_data_dir = match (&local_data_override, &data_override) {
            (Some(path), _) | (None, Some(path)) => path.clone(),
            (None, None) => default_root(
                environment,
                AppPathRoot::LocalData,
                &environment.local_data_base,
            )?,
        };
        let state_dir = match (&state_override, &local_data_override, &data_override) {
            (Some(path), _, _) | (None, Some(path), _) | (None, None, Some(path)) => path.clone(),
            (None, None, None) => match platform {
                PathPlatform::Linux => {
                    default_root(environment, AppPathRoot::State, &environment.state_base)?
                }
                PathPlatform::MacOs | PathPlatform::Windows => {
                    join_component(platform, &local_data_dir, "state")
                }
            },
        };
        let cache_dir = match resolve_override(
            environment,
            "ZS_CACHE_DIR",
            environment.overrides.cache_dir.as_deref(),
        )? {
            Some(path) => path,
            None => {
                let base = required_base(
                    platform,
                    AppPathRoot::Cache,
                    environment.cache_base.as_deref(),
                )?;
                let application = join_component(platform, base, APP_COMPONENT);
                match platform {
                    PathPlatform::Windows => join_component(platform, &application, "cache"),
                    PathPlatform::Linux | PathPlatform::MacOs => application,
                }
            }
        };
        let credentials_dir = match resolve_override(
            environment,
            "ZS_CREDENTIALS_DIR",
            environment.overrides.credentials_dir.as_deref(),
        )? {
            Some(path) => path,
            None => join_component(platform, &local_data_dir, "credentials"),
        };
        let project_dir = environment
            .workspace_root
            .as_deref()
            .map(|root| {
                ensure_absolute(platform, AppPathRoot::Workspace, root)?;
                Ok(join_component(
                    platform,
                    root,
                    crate::product::LEGACY_PROJECT_DIRECTORY,
                ))
            })
            .transpose()?;

        Ok(Self {
            config_dir,
            data_dir,
            local_data_dir,
            state_dir,
            cache_dir,
            credentials_dir,
            project_dir,
        })
    }

    /// Rebind only workspace-owned application storage while retaining the
    /// immutable process-wide config/data/cache roots selected at startup.
    pub fn with_workspace_root(&self, workspace_root: &Path) -> Result<Self, AppPathError> {
        let platform = PathPlatform::current()?;
        ensure_absolute(platform, AppPathRoot::Workspace, workspace_root)?;
        let mut paths = self.clone();
        paths.project_dir = Some(join_component(platform, workspace_root, ".zerostack"));
        Ok(paths)
    }

    #[allow(dead_code)]
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn suffix_file(&self) -> PathBuf {
        self.config_dir.join("SUFFIX.md")
    }

    pub fn global_agents_file(&self) -> PathBuf {
        self.config_dir.join("agent").join("AGENTS.md")
    }

    #[cfg(feature = "archmd")]
    pub fn global_architecture_file(&self) -> PathBuf {
        self.config_dir.join("agent").join("ARCHITECTURE.md")
    }

    #[allow(dead_code)]
    pub fn global_hook_settings_file(&self) -> PathBuf {
        self.config_dir.join("settings.json")
    }

    pub fn project_config_file(&self) -> Option<PathBuf> {
        self.project_dir
            .as_ref()
            .map(|directory| directory.join("config.toml"))
    }

    pub fn project_prompts_dir(&self) -> Option<PathBuf> {
        self.project_dir
            .as_ref()
            .map(|directory| directory.join("prompts"))
    }

    #[allow(dead_code)]
    pub fn project_agent_skills_dir(&self) -> Option<PathBuf> {
        self.project_dir
            .as_ref()
            .map(|directory| directory.join("skills"))
    }

    #[allow(dead_code)]
    pub fn project_hook_settings_file(&self) -> Option<PathBuf> {
        self.project_dir
            .as_ref()
            .map(|directory| directory.join("settings.json"))
    }

    pub fn prompts_dir(&self) -> PathBuf {
        self.data_dir.join("prompts")
    }

    pub fn themes_dir(&self) -> PathBuf {
        self.data_dir.join("themes")
    }

    pub fn docs_dir(&self) -> PathBuf {
        self.data_dir.join("docs")
    }

    pub fn memory_dir(&self) -> PathBuf {
        self.data_dir.join("memory")
    }

    #[allow(dead_code)]
    pub fn portable_agent_skills_dir(&self) -> PathBuf {
        self.data_dir.join("skills")
    }

    pub fn learned_skills_dir(&self) -> PathBuf {
        self.local_data_dir.join("skills")
    }

    pub fn learned_skills_db(&self) -> PathBuf {
        self.learned_skills_dir().join("skills.db")
    }

    #[allow(dead_code)]
    pub fn embedding_models_dir(&self) -> PathBuf {
        self.cache_dir.join("models")
    }

    #[allow(dead_code)]
    pub fn learned_skills_cache_dir(&self) -> PathBuf {
        self.cache_dir.join("skills")
    }

    #[allow(dead_code)]
    pub fn import_staging_dir(&self) -> PathBuf {
        self.cache_dir.join("import-staging")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.state_dir.join("sessions")
    }

    pub fn tool_outputs_dir(&self) -> PathBuf {
        self.state_dir.join("tool-outputs")
    }

    pub fn transcripts_dir(&self) -> PathBuf {
        self.state_dir.join("loops")
    }

    #[allow(dead_code)]
    pub fn turn_telemetry_dir(&self) -> PathBuf {
        self.state_dir.join("telemetry")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.state_dir.join("logs")
    }

    pub fn crash_logs_dir(&self) -> PathBuf {
        self.logs_dir().join("crashes")
    }

    pub fn chat_history_file(&self) -> PathBuf {
        self.state_dir.join("chat_history.json")
    }

    pub fn welcome_marker_file(&self) -> PathBuf {
        self.state_dir.join("shown_welcome_msg")
    }

    pub fn hook_trust_file(&self) -> PathBuf {
        self.state_dir.join("hooks").join("trusted-hooks.json")
    }

    pub fn project_config_trust_file(&self) -> PathBuf {
        self.state_dir
            .join("config")
            .join("trusted-project-configs.json")
    }

    #[allow(dead_code)]
    pub fn mcp_oauth_dir(&self) -> PathBuf {
        self.credentials_dir.join("mcp-oauth")
    }

    pub fn archmd_state_dir(&self) -> PathBuf {
        self.state_dir.join("archmd")
    }

    pub fn theme_selection_file(&self) -> PathBuf {
        self.data_dir.join("theme.json")
    }

    pub fn migration_markers_dir(&self) -> PathBuf {
        self.state_dir.join("migrations").join("v1")
    }

    pub fn effect_audit(&self) -> EffectAuditPathOwner {
        EffectAuditPathOwner {
            state_root: self.state_dir.clone(),
            directory: self.state_dir.join("audit").join("js-effects"),
        }
    }
}

/// Installs the immutable roots resolved by startup for process-wide artifact
/// owners that cannot carry an `AppPaths` reference in their public API.
pub fn install_process_paths(paths: &AppPaths) -> Result<(), AppPathError> {
    match PROCESS_PATHS.set(paths.clone()) {
        Ok(()) => Ok(()),
        Err(candidate) if PROCESS_PATHS.get() == Some(&candidate) => Ok(()),
        Err(_) => Err(AppPathError::AlreadyInitialized),
    }
}

/// Returns the roots installed by startup.
///
/// Unit tests retain a process-derived fallback while older tests migrate to
/// explicit fixtures. Production never re-reads path environment variables.
pub fn process_paths() -> Result<AppPaths, AppPathError> {
    if let Some(paths) = PROCESS_PATHS.get() {
        return Ok(paths.clone());
    }
    #[cfg(test)]
    {
        AppPaths::from_process(std::env::current_dir().ok())
    }
    #[cfg(not(test))]
    {
        Err(AppPathError::NotInitialized)
    }
}

pub fn process_home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

pub fn prepare_storage_roots(paths: &AppPaths) -> io::Result<()> {
    for root in [
        &paths.config_dir,
        &paths.data_dir,
        &paths.local_data_dir,
        &paths.state_dir,
        &paths.cache_dir,
        &paths.credentials_dir,
    ] {
        create_private_dir(root)?;
    }
    Ok(())
}

pub(crate) fn ensure_private_directory(path: &Path) -> io::Result<()> {
    create_private_dir(path)
}

pub fn artifact_disabled(artifact: &'static str) -> bool {
    DISABLED_ARTIFACTS
        .get()
        .and_then(|disabled| disabled.lock().ok())
        .is_some_and(|disabled| disabled.contains(artifact))
}

fn disable_artifact(artifact: &'static str) {
    let disabled = DISABLED_ARTIFACTS.get_or_init(|| Mutex::new(HashSet::new()));
    if let Ok(mut disabled) = disabled.lock() {
        disabled.insert(artifact);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyArtifactRequirement {
    Required,
    Optional,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyArtifactKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyConflict {
    pub artifact: &'static str,
    pub canonical: PathBuf,
    pub candidates: Vec<PathBuf>,
    pub requirement: LegacyArtifactRequirement,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyMigrationStatus {
    NoLegacyContent,
    CanonicalPresent,
    Migrated { source: PathBuf },
    DisabledByConflict(LegacyConflict),
}

#[derive(Debug, thiserror::Error)]
pub enum LegacyMigrationError {
    #[error(
        "legacy {artifact} conflict: canonical path {canonical:?}; choose one of {candidates:?}"
    )]
    Conflict {
        artifact: &'static str,
        canonical: PathBuf,
        candidates: Vec<PathBuf>,
    },
    #[error("invalid legacy selection {selected:?} for {artifact}")]
    InvalidSelection {
        artifact: &'static str,
        selected: PathBuf,
    },
    #[error("legacy {artifact} migration failed at {path:?}: {source}")]
    Io {
        artifact: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("legacy {artifact} changed while it was being migrated")]
    Changed { artifact: &'static str },
    #[error("legacy {artifact} verification failed")]
    Verification { artifact: &'static str },
}

#[derive(Debug, Clone)]
pub struct LegacyMigrationRequest {
    pub artifact: &'static str,
    pub canonical: PathBuf,
    pub candidates: Vec<PathBuf>,
    pub marker: PathBuf,
    pub requirement: LegacyArtifactRequirement,
    pub kind: LegacyArtifactKind,
    pub selected: Option<PathBuf>,
}

#[derive(serde::Serialize)]
struct MigrationMarker {
    version: u32,
    artifact: String,
    source: String,
    canonical: String,
    content_sha256: String,
}

pub fn migrate_legacy_path(
    request: &LegacyMigrationRequest,
) -> Result<LegacyMigrationStatus, LegacyMigrationError> {
    if path_exists_no_follow(&request.canonical, request.artifact, request.kind)? {
        if !path_exists_no_follow(&request.marker, request.artifact, LegacyArtifactKind::File)? {
            let canonical_identity = content_identity(&request.canonical).map_err(|source| {
                LegacyMigrationError::Io {
                    artifact: request.artifact,
                    path: request.canonical.clone(),
                    source,
                }
            })?;
            for candidate in
                existing_candidates(&request.candidates, request.artifact, request.kind)?
            {
                let candidate_identity =
                    content_identity(&candidate).map_err(|source| LegacyMigrationError::Io {
                        artifact: request.artifact,
                        path: candidate.clone(),
                        source,
                    })?;
                if candidate_identity == canonical_identity {
                    write_migration_marker(request, &candidate, &canonical_identity)?;
                    break;
                }
            }
        }
        return Ok(LegacyMigrationStatus::CanonicalPresent);
    }

    let mut candidates = existing_candidates(&request.candidates, request.artifact, request.kind)?;
    if candidates.is_empty() {
        return Ok(LegacyMigrationStatus::NoLegacyContent);
    }
    candidates.sort();
    candidates.dedup();

    let identities = candidates
        .iter()
        .map(|path| {
            content_identity(path)
                .map(|identity| (path.clone(), identity))
                .map_err(|source| LegacyMigrationError::Io {
                    artifact: request.artifact,
                    path: path.clone(),
                    source,
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let all_identical = identities.windows(2).all(|pair| pair[0].1 == pair[1].1);

    let source = if let Some(selected) = request.selected.as_ref() {
        identities
            .iter()
            .find(|(path, _)| path == selected)
            .map(|(path, _)| path.clone())
            .ok_or_else(|| LegacyMigrationError::InvalidSelection {
                artifact: request.artifact,
                selected: selected.clone(),
            })?
    } else if identities.len() == 1 || all_identical {
        identities[0].0.clone()
    } else {
        let conflict = LegacyConflict {
            artifact: request.artifact,
            canonical: request.canonical.clone(),
            candidates: candidates.clone(),
            requirement: request.requirement,
        };
        return match request.requirement {
            LegacyArtifactRequirement::Required => Err(LegacyMigrationError::Conflict {
                artifact: request.artifact,
                canonical: request.canonical.clone(),
                candidates,
            }),
            LegacyArtifactRequirement::Optional => {
                Ok(LegacyMigrationStatus::DisabledByConflict(conflict))
            }
        };
    };

    let source_identity =
        content_identity(&source).map_err(|source_error| LegacyMigrationError::Io {
            artifact: request.artifact,
            path: source.clone(),
            source: source_error,
        })?;
    copy_verified(&source, &request.canonical, request.artifact, request.kind)?;
    let canonical_identity =
        content_identity(&request.canonical).map_err(|source_error| LegacyMigrationError::Io {
            artifact: request.artifact,
            path: request.canonical.clone(),
            source: source_error,
        })?;
    if source_identity != canonical_identity {
        remove_created_canonical(&request.canonical);
        return Err(LegacyMigrationError::Verification {
            artifact: request.artifact,
        });
    }
    let source_after =
        content_identity(&source).map_err(|source_error| LegacyMigrationError::Io {
            artifact: request.artifact,
            path: source.clone(),
            source: source_error,
        })?;
    if source_after != source_identity {
        remove_created_canonical(&request.canonical);
        return Err(LegacyMigrationError::Changed {
            artifact: request.artifact,
        });
    }

    write_migration_marker(request, &source, &source_identity)?;

    Ok(LegacyMigrationStatus::Migrated { source })
}

fn write_migration_marker(
    request: &LegacyMigrationRequest,
    source_path: &Path,
    content_sha256: &str,
) -> Result<(), LegacyMigrationError> {
    let marker = serde_json::to_vec_pretty(&MigrationMarker {
        version: MIGRATION_VERSION,
        artifact: request.artifact.to_string(),
        source: source_path.to_string_lossy().into_owned(),
        canonical: request.canonical.to_string_lossy().into_owned(),
        content_sha256: content_sha256.to_string(),
    })
    .map_err(|source| LegacyMigrationError::Io {
        artifact: request.artifact,
        path: request.marker.clone(),
        source: io::Error::other(source),
    })?;
    if let Some(parent) = request.marker.parent() {
        create_private_dir(parent).map_err(|source| LegacyMigrationError::Io {
            artifact: request.artifact,
            path: parent.to_path_buf(),
            source,
        })?;
    }
    crate::fs::atomic_write_sync(&request.marker, &marker).map_err(|source| {
        LegacyMigrationError::Io {
            artifact: request.artifact,
            path: request.marker.clone(),
            source,
        }
    })?;
    Ok(())
}

pub fn converge_legacy_artifacts(
    paths: &AppPaths,
    interactive: bool,
) -> Result<Vec<LegacyMigrationStatus>, LegacyMigrationError> {
    let mut statuses = Vec::new();
    statuses.push(converge_legacy_config(paths, interactive)?);

    let legacy_data_roots = documented_legacy_data_roots(paths);
    let legacy_config_roots = documented_legacy_config_roots(paths);
    let optional: Vec<(&'static str, PathBuf, Vec<PathBuf>, LegacyArtifactKind)> = vec![
        (
            "sessions",
            paths.sessions_dir(),
            legacy_data_roots
                .iter()
                .map(|root| root.join("sessions"))
                .collect(),
            LegacyArtifactKind::Directory,
        ),
        (
            "tool outputs",
            paths.tool_outputs_dir(),
            legacy_data_roots
                .iter()
                .map(|root| root.join("tool-outputs"))
                .collect(),
            LegacyArtifactKind::Directory,
        ),
        (
            "loop transcripts",
            paths.transcripts_dir(),
            legacy_data_roots
                .iter()
                .map(|root| root.join("loops"))
                .collect(),
            LegacyArtifactKind::Directory,
        ),
        (
            "logs",
            paths.logs_dir(),
            legacy_data_roots
                .iter()
                .map(|root| root.join("logs"))
                .collect(),
            LegacyArtifactKind::Directory,
        ),
        (
            "chat history",
            paths.chat_history_file(),
            legacy_data_roots
                .iter()
                .map(|root| root.join("chat_history.json"))
                .collect(),
            LegacyArtifactKind::File,
        ),
        (
            "welcome marker",
            paths.welcome_marker_file(),
            legacy_data_roots
                .iter()
                .map(|root| root.join("shown_welcome_msg"))
                .collect(),
            LegacyArtifactKind::File,
        ),
        (
            "hook trust",
            paths.hook_trust_file(),
            legacy_data_roots
                .iter()
                .map(|root| root.join("trusted-hooks.json"))
                .collect(),
            LegacyArtifactKind::File,
        ),
        (
            "architecture prompt state",
            paths.archmd_state_dir().join("dirs_asked_architecture.txt"),
            legacy_data_roots
                .iter()
                .map(|root| root.join("dirs_asked_architecture.txt"))
                .collect(),
            LegacyArtifactKind::File,
        ),
        (
            "memory",
            paths.memory_dir(),
            legacy_config_roots
                .iter()
                .map(|root| root.join("agent").join("memory"))
                .collect(),
            LegacyArtifactKind::Directory,
        ),
        (
            "learned JS skill database",
            paths.learned_skills_db(),
            legacy_config_roots
                .iter()
                .flat_map(|root| [root.join("skills.db"), root.join("agent").join("skills.db")])
                .chain(legacy_data_roots.iter().flat_map(|root| {
                    [
                        root.join("skills.db"),
                        root.join("skills").join("skills.db"),
                    ]
                }))
                .collect(),
            LegacyArtifactKind::File,
        ),
    ];

    for (artifact, canonical, candidates, kind) in optional {
        let marker = paths
            .migration_markers_dir()
            .join(format!("{}.json", artifact.replace(' ', "-")));
        let mut request = LegacyMigrationRequest {
            artifact,
            canonical,
            candidates,
            marker,
            requirement: LegacyArtifactRequirement::Optional,
            kind,
            selected: None,
        };
        let mut status = migrate_legacy_path(&request)?;
        if interactive
            && let LegacyMigrationStatus::DisabledByConflict(conflict) = &status
            && let Some(selected) = prompt_legacy_selection(conflict)
        {
            request.selected = Some(selected);
            status = migrate_legacy_path(&request)?;
        }
        if let LegacyMigrationStatus::DisabledByConflict(conflict) = &status {
            disable_artifact(conflict.artifact);
            tracing::warn!(
                "legacy {} conflict disables this optional feature; candidates: {:?}",
                conflict.artifact,
                conflict.candidates
            );
        }
        statuses.push(status);
    }
    Ok(statuses)
}

fn converge_legacy_config(
    paths: &AppPaths,
    interactive: bool,
) -> Result<LegacyMigrationStatus, LegacyMigrationError> {
    for name in ["config.toml", "config.yaml", "config.yml", "config.json"] {
        if path_exists_no_follow(
            &paths.config_dir.join(name),
            "configuration",
            LegacyArtifactKind::File,
        )? {
            return Ok(LegacyMigrationStatus::CanonicalPresent);
        }
    }
    let mut legacy_roots = documented_legacy_config_roots(paths);
    legacy_roots.extend(documented_legacy_data_roots(paths));
    legacy_roots.sort();
    legacy_roots.dedup();
    let candidates = legacy_roots
        .iter()
        .flat_map(|root| {
            ["config.toml", "config.yaml", "config.yml", "config.json"].map(|name| root.join(name))
        })
        .collect::<Vec<_>>();
    let existing = existing_candidates(&candidates, "configuration", LegacyArtifactKind::File)?;
    let canonical_name = existing
        .first()
        .and_then(|path| path.file_name())
        .unwrap_or_else(|| OsStr::new("config.toml"));
    let mut request = LegacyMigrationRequest {
        artifact: "configuration",
        canonical: paths.config_dir.join(canonical_name),
        candidates,
        marker: paths.migration_markers_dir().join("configuration.json"),
        requirement: LegacyArtifactRequirement::Required,
        kind: LegacyArtifactKind::File,
        selected: None,
    };
    if existing.len() > 1 {
        let identities = existing
            .iter()
            .map(|path| {
                content_identity(path).map_err(|source| LegacyMigrationError::Io {
                    artifact: "configuration",
                    path: path.clone(),
                    source,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if identities.windows(2).all(|pair| pair[0] == pair[1]) {
            request.selected = existing.first().cloned();
        }
    }
    match migrate_legacy_path(&request) {
        Err(LegacyMigrationError::Conflict {
            artifact,
            canonical,
            candidates,
        }) if interactive => {
            let conflict = LegacyConflict {
                artifact,
                canonical,
                candidates,
                requirement: LegacyArtifactRequirement::Required,
            };
            if let Some(selected) = prompt_legacy_selection(&conflict) {
                request.canonical = paths.config_dir.join(
                    selected
                        .file_name()
                        .unwrap_or_else(|| OsStr::new("config.toml")),
                );
                request.selected = Some(selected);
                migrate_legacy_path(&request)
            } else {
                Err(LegacyMigrationError::Conflict {
                    artifact: conflict.artifact,
                    canonical: conflict.canonical,
                    candidates: conflict.candidates,
                })
            }
        }
        result => result,
    }
}

fn documented_legacy_config_roots(paths: &AppPaths) -> Vec<PathBuf> {
    let mut roots = vec![paths.config_dir.clone()];
    if let Some(root) = dirs::config_dir() {
        roots.push(root.join(APP_COMPONENT));
    }
    roots.sort();
    roots.dedup();
    roots
}

fn documented_legacy_data_roots(paths: &AppPaths) -> Vec<PathBuf> {
    let mut roots = vec![paths.data_dir.clone()];
    if let Some(root) = dirs::data_dir() {
        roots.push(root.join(APP_COMPONENT));
    }
    roots.sort();
    roots.dedup();
    roots
}

fn prompt_legacy_selection(conflict: &LegacyConflict) -> Option<PathBuf> {
    use std::io::Write;

    eprintln!(
        "Conflicting legacy {} files were found. Select one to migrate:",
        conflict.artifact
    );
    for (index, candidate) in conflict.candidates.iter().enumerate() {
        eprintln!("  {}. {}", index + 1, candidate.display());
    }
    eprint!("Selection (blank cancels): ");
    io::stderr().flush().ok()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input).ok()?;
    let index = input.trim().parse::<usize>().ok()?;
    conflict.candidates.get(index.checked_sub(1)?).cloned()
}

fn existing_candidates(
    candidates: &[PathBuf],
    artifact: &'static str,
    kind: LegacyArtifactKind,
) -> Result<Vec<PathBuf>, LegacyMigrationError> {
    let mut existing = Vec::new();
    for candidate in candidates {
        if path_exists_no_follow(candidate, artifact, kind)? {
            existing.push(candidate.clone());
        }
    }
    Ok(existing)
}

fn path_exists_no_follow(
    path: &Path,
    artifact: &'static str,
    kind: LegacyArtifactKind,
) -> Result<bool, LegacyMigrationError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if portable::is_link_or_reparse(&metadata) => Err(LegacyMigrationError::Io {
            artifact,
            path: path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::PermissionDenied,
                "legacy paths must not be symbolic links",
            ),
        }),
        Ok(metadata)
            if (matches!(kind, LegacyArtifactKind::File) && metadata.is_file())
                || (matches!(kind, LegacyArtifactKind::Directory) && metadata.is_dir()) =>
        {
            Ok(true)
        }
        Ok(_) => Err(LegacyMigrationError::Io {
            artifact,
            path: path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidData,
                "legacy artifact has the wrong filesystem type",
            ),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(LegacyMigrationError::Io {
            artifact,
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn content_identity(path: &Path) -> io::Result<String> {
    use sha2::{Digest, Sha256};

    reject_link_components(path)?;
    if let Some(parent) = path.parent() {
        reject_link_components(parent)?;
    }

    fn update(hasher: &mut Sha256, root: &Path, path: &Path) -> io::Result<()> {
        let metadata = std::fs::symlink_metadata(path)?;
        if portable::is_link_or_reparse(&metadata) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "legacy migration does not follow links",
            ));
        }
        let relative = path.strip_prefix(root).unwrap_or(path);
        hasher.update(relative.to_string_lossy().as_bytes());
        if metadata.is_file() {
            hasher.update([0]);
            let mut file = open_regular_no_follow(path)?;
            let mut buffer = [0_u8; 16 * 1024];
            loop {
                let read = file.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
        } else if metadata.is_dir() {
            hasher.update([1]);
            let mut entries = std::fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                update(hasher, root, &entry.path())?;
            }
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "legacy artifact is not a regular file or directory",
            ));
        }
        Ok(())
    }

    let mut hasher = Sha256::new();
    update(&mut hasher, path, path)?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn open_regular_no_follow(path: &Path) -> io::Result<std::fs::File> {
    let before = crate::fs::checked_path_metadata(path)?;
    if portable::is_link_or_reparse(&before) || !before.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "legacy artifact must be a regular file",
        ));
    }
    let file = std::fs::File::open(path)?;
    let opened = crate::fs::checked_file_metadata(&file)?;
    let after = crate::fs::checked_path_metadata(path)?;
    crate::fs::ensure_same_file(path, &before, &opened)?;
    crate::fs::ensure_same_file(path, &opened, &after)?;
    Ok(file)
}

fn copy_verified(
    source: &Path,
    canonical: &Path,
    artifact: &'static str,
    kind: LegacyArtifactKind,
) -> Result<(), LegacyMigrationError> {
    let metadata =
        std::fs::symlink_metadata(source).map_err(|source_error| LegacyMigrationError::Io {
            artifact,
            path: source.to_path_buf(),
            source: source_error,
        })?;
    if matches!(kind, LegacyArtifactKind::File) && metadata.is_file() {
        let mut input =
            open_regular_no_follow(source).map_err(|source_error| LegacyMigrationError::Io {
                artifact,
                path: source.to_path_buf(),
                source: source_error,
            })?;
        let mut bytes = Vec::new();
        input
            .read_to_end(&mut bytes)
            .map_err(|source_error| LegacyMigrationError::Io {
                artifact,
                path: source.to_path_buf(),
                source: source_error,
            })?;
        if let Some(parent) = canonical.parent() {
            create_private_dir(parent).map_err(|source_error| LegacyMigrationError::Io {
                artifact,
                path: parent.to_path_buf(),
                source: source_error,
            })?;
        }
        match crate::fs::atomic_create_sync(canonical, &bytes) {
            Ok(()) => Ok(()),
            Err(_) if existing_content_is_identical(source, canonical) => Ok(()),
            Err(source_error) => Err(LegacyMigrationError::Io {
                artifact,
                path: canonical.to_path_buf(),
                source: source_error,
            }),
        }
    } else if matches!(kind, LegacyArtifactKind::Directory) && metadata.is_dir() {
        copy_directory_verified(source, canonical, artifact)
    } else {
        Err(LegacyMigrationError::Io {
            artifact,
            path: source.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidData,
                "legacy artifact is not a regular file or directory",
            ),
        })
    }
}

fn remove_created_canonical(path: &Path) {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    if portable::is_link_or_reparse(&metadata) {
        return;
    }
    if metadata.is_dir() {
        let _ = std::fs::remove_dir_all(path);
    } else if metadata.is_file() {
        let _ = std::fs::remove_file(path);
    }
}

fn copy_directory_verified(
    source: &Path,
    canonical: &Path,
    artifact: &'static str,
) -> Result<(), LegacyMigrationError> {
    let parent = canonical.parent().ok_or_else(|| LegacyMigrationError::Io {
        artifact,
        path: canonical.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "canonical path has no parent"),
    })?;
    create_private_dir(parent).map_err(|source| LegacyMigrationError::Io {
        artifact,
        path: parent.to_path_buf(),
        source,
    })?;
    let stage = parent.join(format!(".migration-{}", uuid::Uuid::new_v4()));
    create_private_dir(&stage).map_err(|source| LegacyMigrationError::Io {
        artifact,
        path: stage.clone(),
        source,
    })?;

    fn copy_tree(source: &Path, target: &Path) -> io::Result<()> {
        let mut entries = std::fs::read_dir(source)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let source_path = entry.path();
            let metadata = std::fs::symlink_metadata(&source_path)?;
            if portable::is_link_or_reparse(&metadata) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "legacy migration does not follow links",
                ));
            }
            let target_path = target.join(entry.file_name());
            if metadata.is_dir() {
                create_private_dir(&target_path)?;
                copy_tree(&source_path, &target_path)?;
            } else if metadata.is_file() {
                let mut input = open_regular_no_follow(&source_path)?;
                let mut bytes = Vec::new();
                input.read_to_end(&mut bytes)?;
                crate::fs::atomic_create_sync(&target_path, &bytes)?;
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "legacy artifact contains a special file",
                ));
            }
        }
        Ok(())
    }

    let copy_result = copy_tree(source, &stage).and_then(|()| {
        if content_identity(source)? != content_identity(&stage)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "staged migration verification failed",
            ));
        }
        sync_directory_tree(&stage)?;
        publish_staged_directory(&stage, canonical)?;
        sync_directory(parent)
    });
    match copy_result {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_dir_all(&stage);
            if existing_content_is_identical(source, canonical) {
                Ok(())
            } else {
                Err(LegacyMigrationError::Io {
                    artifact,
                    path: canonical.to_path_buf(),
                    source: error,
                })
            }
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn publish_staged_directory(stage: &Path, canonical: &Path) -> io::Result<()> {
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_RENAME_INFORMATION, FileRenameInformation, NtSetInformationFile,
    };
    use windows_sys::Win32::Foundation::RtlNtStatusToDosError;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    let stage_parent = stage
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "stage has no parent"))?;
    let canonical_parent = canonical
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target has no parent"))?;
    if stage_parent != canonical_parent {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "staged directory and target do not share a parent",
        ));
    }
    let parent_identity = crate::fs::checked_path_metadata(canonical_parent)?;
    if portable::is_link_or_reparse(&parent_identity) || !parent_identity.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "migration parent is not a real directory",
        ));
    }
    let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    let destination_parent = std::fs::OpenOptions::new()
        .access_mode(FILE_TRAVERSE | FILE_READ_ATTRIBUTES)
        .share_mode(share)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(canonical_parent)?;
    crate::fs::ensure_same_file(
        canonical_parent,
        &parent_identity,
        &crate::fs::checked_file_metadata(&destination_parent)?,
    )?;
    let stage_identity = crate::fs::checked_path_metadata(stage)?;
    if portable::is_link_or_reparse(&stage_identity) || !stage_identity.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "migration stage is not a real directory",
        ));
    }
    match std::fs::symlink_metadata(canonical) {
        Ok(metadata) if portable::is_link_or_reparse(&metadata) || !metadata.is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "migration target is not a real directory",
            ));
        }
        Ok(_) => {
            // Windows cannot rename a directory over even an empty directory.
            // Root initialization may have created this exact empty target;
            // remove_dir is intentionally non-recursive, so a concurrent
            // writer makes this fail without losing any content.
            std::fs::remove_dir(canonical)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let source = std::fs::OpenOptions::new()
        .access_mode(DELETE)
        .share_mode(share)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(stage)?;
    crate::fs::ensure_same_file(
        stage,
        &stage_identity,
        &crate::fs::checked_file_metadata(&source)?,
    )?;
    let target_name = canonical
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target has no filename"))?
        .encode_wide()
        .collect::<Vec<_>>();
    let name_bytes = target_name
        .len()
        .checked_mul(size_of::<u16>())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "migration target name is too long",
            )
        })?;
    let header_bytes = std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName);
    let total_bytes = header_bytes
        .checked_add(name_bytes)
        .and_then(|bytes| bytes.checked_add(size_of::<u16>()))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "migration rename buffer is too large",
            )
        })?;
    let mut storage = vec![0usize; total_bytes.div_ceil(size_of::<usize>())];
    let information = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    crate::fs::ensure_same_file(
        canonical_parent,
        &parent_identity,
        &crate::fs::checked_path_metadata(canonical_parent)?,
    )?;
    // SAFETY: `storage` is pointer-aligned and sized for the fixed header, the
    // complete UTF-16 target name, and its trailing zero. The source and
    // identity-verified destination-parent handles and buffer remain live for
    // the synchronous system call.
    let mut io_status = IO_STATUS_BLOCK::default();
    unsafe {
        (*information).Anonymous.ReplaceIfExists = false;
        (*information).RootDirectory = destination_parent.as_raw_handle().cast();
        (*information).FileNameLength = u32::try_from(name_bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "migration target name is too long",
            )
        })?;
        std::ptr::copy_nonoverlapping(
            target_name.as_ptr(),
            std::ptr::addr_of_mut!((*information).FileName).cast::<u16>(),
            target_name.len(),
        );
        let status = NtSetInformationFile(
            source.as_raw_handle().cast(),
            &mut io_status,
            information.cast(),
            u32::try_from(total_bytes).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "migration rename buffer is too large",
                )
            })?,
            FileRenameInformation,
        );
        if status < 0 {
            return Err(io::Error::from_raw_os_error(
                RtlNtStatusToDosError(status) as i32
            ));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn publish_staged_directory(stage: &Path, canonical: &Path) -> io::Result<()> {
    std::fs::rename(stage, canonical)
}

fn existing_content_is_identical(left: &Path, right: &Path) -> bool {
    match (content_identity(left), content_identity(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn sync_directory_tree(path: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            sync_directory_tree(&entry.path())?;
        }
    }
    sync_directory(path)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    reject_link_components(path)?;
    crate::fs::ensure_private_directory(path)
}

fn reject_link_components(path: &Path) -> io::Result<()> {
    for component in path.ancestors() {
        match std::fs::symlink_metadata(component) {
            Ok(metadata) if portable::is_link_or_reparse(&metadata) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "private paths {PRIVATE_PATH_LINK_POLICY}: {}",
                        component.display()
                    ),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn default_root(
    environment: &PathEnvironment,
    root: AppPathRoot,
    base: &Option<PathBuf>,
) -> Result<PathBuf, AppPathError> {
    let base = required_base(environment.platform, root, base.as_deref())?;
    Ok(join_component(environment.platform, base, APP_COMPONENT))
}

fn required_base(
    platform: PathPlatform,
    root: AppPathRoot,
    base: Option<&Path>,
) -> Result<&Path, AppPathError> {
    let base = base.ok_or(AppPathError::MissingBase { root, platform })?;
    ensure_absolute(platform, root, base)?;
    Ok(base)
}

fn resolve_override(
    environment: &PathEnvironment,
    variable: &'static str,
    value: Option<&OsStr>,
) -> Result<Option<PathBuf>, AppPathError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() {
        return Err(AppPathError::EmptyOverride { variable });
    }

    let path = expand_tilde(
        environment.platform,
        environment.home_dir.as_deref(),
        variable,
        value,
    )?;
    if !is_absolute(environment.platform, &path) {
        return Err(AppPathError::RelativeOverride {
            variable,
            value: path,
        });
    }
    Ok(Some(path))
}

fn expand_tilde(
    platform: PathPlatform,
    home_dir: Option<&Path>,
    variable: &'static str,
    value: &OsStr,
) -> Result<PathBuf, AppPathError> {
    let text = value.to_string_lossy();
    let suffix = if text == "~" {
        Some("")
    } else if let Some(suffix) = text.strip_prefix("~/") {
        Some(suffix)
    } else {
        text.strip_prefix("~\\")
    };
    let Some(suffix) = suffix else {
        return Ok(PathBuf::from(value));
    };
    let home_dir = home_dir.ok_or(AppPathError::MissingHomeForTilde { variable })?;
    if suffix.is_empty() {
        return Ok(home_dir.to_path_buf());
    }
    match platform {
        PathPlatform::Windows => Ok(join_component(
            platform,
            home_dir,
            &suffix.replace('/', "\\"),
        )),
        PathPlatform::Linux | PathPlatform::MacOs => Ok(join_component(platform, home_dir, suffix)),
    }
}

fn ensure_absolute(
    platform: PathPlatform,
    root: AppPathRoot,
    value: &Path,
) -> Result<(), AppPathError> {
    if is_absolute(platform, value) {
        Ok(())
    } else {
        Err(AppPathError::RelativeBase {
            root,
            value: value.to_path_buf(),
        })
    }
}

fn is_absolute(platform: PathPlatform, path: &Path) -> bool {
    match platform {
        PathPlatform::Linux | PathPlatform::MacOs => {
            path.as_os_str().to_string_lossy().starts_with('/')
        }
        PathPlatform::Windows => {
            let value = path.as_os_str().to_string_lossy();
            let value = value.as_bytes();
            let has_drive_root = value.len() >= 3
                && value[0].is_ascii_alphabetic()
                && value[1] == b':'
                && matches!(value[2], b'\\' | b'/');
            let has_unc_root =
                value.len() >= 2 && matches!(value[0], b'\\' | b'/') && value[1] == value[0];
            has_drive_root || has_unc_root
        }
    }
}

fn join_component(platform: PathPlatform, base: &Path, component: &str) -> PathBuf {
    let separator = match platform {
        PathPlatform::Linux | PathPlatform::MacOs => "/",
        PathPlatform::Windows => "\\",
    };
    let mut value = base.as_os_str().to_os_string();
    let base_text = base.as_os_str().to_string_lossy();
    let has_separator = match platform {
        PathPlatform::Linux | PathPlatform::MacOs => base_text.ends_with('/'),
        PathPlatform::Windows => base_text.ends_with('/') || base_text.ends_with('\\'),
    };
    if !has_separator {
        value.push(separator);
    }
    value.push(component);
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_override(
        overrides: &mut PathOverrides,
        variable: &'static str,
        value: Option<OsString>,
    ) {
        match variable {
            "ZS_CONFIG_DIR" => overrides.config_dir = value,
            "ZS_DATA_DIR" => overrides.data_dir = value,
            "ZS_LOCAL_DATA_DIR" => overrides.local_data_dir = value,
            "ZS_STATE_DIR" => overrides.state_dir = value,
            "ZS_CACHE_DIR" => overrides.cache_dir = value,
            "ZS_CREDENTIALS_DIR" => overrides.credentials_dir = value,
            _ => unreachable!("unknown path override"),
        }
    }

    fn linux_environment() -> PathEnvironment {
        PathEnvironment {
            platform: PathPlatform::Linux,
            home_dir: Some(PathBuf::from("/home/alice")),
            config_base: Some(PathBuf::from("/home/alice/.config")),
            data_base: Some(PathBuf::from("/home/alice/.local/share")),
            local_data_base: Some(PathBuf::from("/home/alice/.local/share")),
            state_base: Some(PathBuf::from("/home/alice/.local/state")),
            cache_base: Some(PathBuf::from("/home/alice/.cache")),
            workspace_root: Some(PathBuf::from("/work/project")),
            overrides: PathOverrides::default(),
        }
    }

    #[test]
    fn app_paths_matrix_linux_defaults_and_project_root() {
        let paths = AppPaths::resolve(&linux_environment()).unwrap();

        assert_eq!(
            paths.config_dir,
            PathBuf::from("/home/alice/.config/zerostack")
        );
        assert_eq!(
            paths.data_dir,
            PathBuf::from("/home/alice/.local/share/zerostack")
        );
        assert_eq!(paths.local_data_dir, paths.data_dir);
        assert_eq!(
            paths.state_dir,
            PathBuf::from("/home/alice/.local/state/zerostack")
        );
        assert_eq!(
            paths.cache_dir,
            PathBuf::from("/home/alice/.cache/zerostack")
        );
        assert_eq!(
            paths.credentials_dir,
            PathBuf::from("/home/alice/.local/share/zerostack/credentials")
        );
        assert_eq!(
            paths.project_dir,
            Some(PathBuf::from("/work/project/.zerostack"))
        );
    }

    #[test]
    fn app_paths_matrix_macos_defaults() {
        let environment = PathEnvironment {
            platform: PathPlatform::MacOs,
            home_dir: Some(PathBuf::from("/Users/alice")),
            config_base: Some(PathBuf::from("/Users/alice/Library/Application Support")),
            data_base: Some(PathBuf::from("/Users/alice/Library/Application Support")),
            local_data_base: Some(PathBuf::from("/Users/alice/Library/Application Support")),
            state_base: None,
            cache_base: Some(PathBuf::from("/Users/alice/Library/Caches")),
            workspace_root: None,
            overrides: PathOverrides::default(),
        };

        let paths = AppPaths::resolve(&environment).unwrap();
        let application_support =
            PathBuf::from("/Users/alice/Library/Application Support/zerostack");
        assert_eq!(paths.config_dir, application_support);
        assert_eq!(paths.data_dir, application_support);
        assert_eq!(paths.local_data_dir, application_support);
        assert_eq!(
            paths.state_dir,
            PathBuf::from("/Users/alice/Library/Application Support/zerostack/state")
        );
        assert_eq!(
            paths.cache_dir,
            PathBuf::from("/Users/alice/Library/Caches/zerostack")
        );
        assert_eq!(
            paths.credentials_dir,
            PathBuf::from("/Users/alice/Library/Application Support/zerostack/credentials")
        );
        assert_eq!(paths.project_dir, None);
    }

    #[test]
    fn app_paths_matrix_windows_defaults_drive_unc_and_long_overrides() {
        let mut environment = PathEnvironment {
            platform: PathPlatform::Windows,
            home_dir: Some(PathBuf::from(r"C:\Users\Alice")),
            config_base: Some(PathBuf::from(r"C:\Users\Alice\AppData\Roaming")),
            data_base: Some(PathBuf::from(r"C:\Users\Alice\AppData\Roaming")),
            local_data_base: Some(PathBuf::from(r"C:\Users\Alice\AppData\Local")),
            state_base: None,
            cache_base: Some(PathBuf::from(r"C:\Users\Alice\AppData\Local")),
            workspace_root: Some(PathBuf::from(r"C:\work\project")),
            overrides: PathOverrides::default(),
        };

        let paths = AppPaths::resolve(&environment).unwrap();
        assert_eq!(
            paths.config_dir,
            PathBuf::from(r"C:\Users\Alice\AppData\Roaming\zerostack")
        );
        assert_eq!(paths.config_dir, paths.data_dir);
        assert_eq!(
            paths.local_data_dir,
            PathBuf::from(r"C:\Users\Alice\AppData\Local\zerostack")
        );
        assert_eq!(
            paths.state_dir,
            PathBuf::from(r"C:\Users\Alice\AppData\Local\zerostack\state")
        );
        assert_eq!(
            paths.cache_dir,
            PathBuf::from(r"C:\Users\Alice\AppData\Local\zerostack\cache")
        );
        assert_eq!(
            paths.credentials_dir,
            PathBuf::from(r"C:\Users\Alice\AppData\Local\zerostack\credentials")
        );
        assert_eq!(
            paths.project_dir,
            Some(PathBuf::from(r"C:\work\project\.zerostack"))
        );

        environment.overrides.data_dir = Some(OsString::from(r"\\server\share\portable"));
        environment.overrides.cache_dir = Some(OsString::from(format!(
            r"C:\{}",
            "long-segment\\".repeat(30)
        )));
        let paths = AppPaths::resolve(&environment).unwrap();
        assert_eq!(paths.data_dir, PathBuf::from(r"\\server\share\portable"));
        assert_eq!(paths.local_data_dir, paths.data_dir);
        assert_eq!(paths.state_dir, paths.data_dir);
        assert!(paths.cache_dir.to_string_lossy().len() > 260);

        environment.overrides.config_dir = Some(OsString::from("~/nested/config"));
        let paths = AppPaths::resolve(&environment).unwrap();
        assert_eq!(
            paths.config_dir,
            PathBuf::from(r"C:\Users\Alice\nested\config")
        );
    }

    #[test]
    fn app_paths_matrix_override_precedence_and_tilde_expansion() {
        let mut environment = linux_environment();
        environment.overrides = PathOverrides {
            config_dir: Some(OsString::from("~/config")),
            data_dir: Some(OsString::from("/legacy")),
            local_data_dir: Some(OsString::from("/local")),
            state_dir: Some(OsString::from("/state")),
            cache_dir: Some(OsString::from("/cache")),
            credentials_dir: Some(OsString::from("/secrets")),
        };

        let paths = AppPaths::resolve(&environment).unwrap();
        assert_eq!(paths.config_dir, PathBuf::from("/home/alice/config"));
        assert_eq!(paths.data_dir, PathBuf::from("/legacy"));
        assert_eq!(paths.local_data_dir, PathBuf::from("/local"));
        assert_eq!(paths.state_dir, PathBuf::from("/state"));
        assert_eq!(paths.cache_dir, PathBuf::from("/cache"));
        assert_eq!(paths.credentials_dir, PathBuf::from("/secrets"));

        environment.overrides.state_dir = None;
        let paths = AppPaths::resolve(&environment).unwrap();
        assert_eq!(paths.state_dir, PathBuf::from("/local"));

        environment.overrides.local_data_dir = None;
        environment.overrides.credentials_dir = None;
        let paths = AppPaths::resolve(&environment).unwrap();
        assert_eq!(paths.local_data_dir, PathBuf::from("/legacy"));
        assert_eq!(paths.state_dir, PathBuf::from("/legacy"));
        assert_eq!(paths.credentials_dir, PathBuf::from("/legacy/credentials"));
        assert_eq!(
            paths.config_dir,
            PathBuf::from("/home/alice/config"),
            "ZS_DATA_DIR must not select the configuration root"
        );

        environment.overrides.config_dir = None;
        let paths = AppPaths::resolve(&environment).unwrap();
        assert_eq!(
            paths.config_dir,
            PathBuf::from("/home/alice/.config/zerostack"),
            "ZS_DATA_DIR must fall through to the platform configuration base"
        );
    }

    #[test]
    fn app_paths_matrix_rejects_invalid_overrides_and_missing_bases() {
        const VARIABLES: [&str; 6] = [
            "ZS_CONFIG_DIR",
            "ZS_DATA_DIR",
            "ZS_LOCAL_DATA_DIR",
            "ZS_STATE_DIR",
            "ZS_CACHE_DIR",
            "ZS_CREDENTIALS_DIR",
        ];
        for variable in VARIABLES {
            let mut environment = linux_environment();
            set_override(&mut environment.overrides, variable, Some(OsString::new()));
            assert_eq!(
                AppPaths::resolve(&environment),
                Err(AppPathError::EmptyOverride { variable })
            );

            let mut environment = linux_environment();
            set_override(
                &mut environment.overrides,
                variable,
                Some(OsString::from("relative/path")),
            );
            assert_eq!(
                AppPaths::resolve(&environment),
                Err(AppPathError::RelativeOverride {
                    variable,
                    value: PathBuf::from("relative/path"),
                })
            );

            let mut environment = linux_environment();
            environment.home_dir = None;
            set_override(
                &mut environment.overrides,
                variable,
                Some(OsString::from("~/path")),
            );
            assert_eq!(
                AppPaths::resolve(&environment),
                Err(AppPathError::MissingHomeForTilde { variable })
            );
        }

        let mut environment = linux_environment();
        environment.config_base = None;
        assert_eq!(
            AppPaths::resolve(&environment),
            Err(AppPathError::MissingBase {
                root: AppPathRoot::Config,
                platform: PathPlatform::Linux,
            })
        );

        environment = linux_environment();
        environment.data_base = None;
        assert_eq!(
            AppPaths::resolve(&environment),
            Err(AppPathError::MissingBase {
                root: AppPathRoot::Data,
                platform: PathPlatform::Linux,
            })
        );

        environment = linux_environment();
        environment.local_data_base = None;
        assert_eq!(
            AppPaths::resolve(&environment),
            Err(AppPathError::MissingBase {
                root: AppPathRoot::LocalData,
                platform: PathPlatform::Linux,
            })
        );

        environment = linux_environment();
        environment.state_base = None;
        assert_eq!(
            AppPaths::resolve(&environment),
            Err(AppPathError::MissingBase {
                root: AppPathRoot::State,
                platform: PathPlatform::Linux,
            })
        );

        environment = linux_environment();
        environment.cache_base = None;
        assert_eq!(
            AppPaths::resolve(&environment),
            Err(AppPathError::MissingBase {
                root: AppPathRoot::Cache,
                platform: PathPlatform::Linux,
            })
        );

        environment = linux_environment();
        environment.cache_base = Some(PathBuf::from("relative/cache"));
        assert_eq!(
            AppPaths::resolve(&environment),
            Err(AppPathError::RelativeBase {
                root: AppPathRoot::Cache,
                value: PathBuf::from("relative/cache"),
            })
        );
    }

    #[test]
    fn app_paths_matrix_routes_startup_config_to_resolved_root() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("zerostack-app-paths-{}", uuid::Uuid::new_v4()));
        let environment = PathEnvironment {
            platform: PathPlatform::current().unwrap(),
            home_dir: Some(root.join("home")),
            config_base: Some(root.join("config-base")),
            data_base: Some(root.join("data-base")),
            local_data_base: Some(root.join("local-data-base")),
            state_base: Some(root.join("state-base")),
            cache_base: Some(root.join("cache-base")),
            workspace_root: Some(root.join("workspace")),
            overrides: PathOverrides {
                config_dir: Some(root.as_os_str().to_os_string()),
                ..PathOverrides::default()
            },
        };
        let paths = AppPaths::resolve(&environment).unwrap();

        let (_, is_first_startup) = crate::config::load_with_paths(&paths, false);

        assert!(is_first_startup);
        assert!(root.join("config.toml").is_file());
        assert!(!root.join("workspace/.zerostack/config.toml").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn isolated_paths() -> (PathBuf, AppPaths) {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("zerostack-path-owner-{}", uuid::Uuid::new_v4()));
        let paths = AppPaths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            local_data_dir: root.join("local"),
            state_dir: root.join("state"),
            cache_dir: root.join("cache"),
            credentials_dir: root.join("credentials"),
            project_dir: Some(root.join("workspace/.zerostack")),
        };
        (root, paths)
    }

    #[test]
    fn windows_directory_publication_uses_a_same_parent_leaf_rename() {
        let source = include_str!("paths.rs");
        let windows_publication = source
            .split("#[cfg(windows)]\n#[allow(unsafe_code)]\nfn publish_staged_directory")
            .nth(1)
            .expect("Windows directory publication implementation");
        let windows_publication = windows_publication
            .split("#[cfg(not(windows))]")
            .next()
            .expect("end of Windows directory publication implementation");

        assert!(windows_publication.contains(".file_name()"));
        assert!(windows_publication.contains("NtSetInformationFile"));
        assert!(
            windows_publication
                .contains("RootDirectory = destination_parent.as_raw_handle().cast()")
        );
        assert!(!windows_publication.contains("canonical.as_os_str().encode_wide()"));
    }

    #[test]
    fn persistent_artifact_ownership_routes_every_class_to_its_typed_root() {
        let (root, paths) = isolated_paths();

        assert!(paths.config_file().starts_with(&paths.config_dir));
        assert!(paths.suffix_file().starts_with(&paths.config_dir));
        assert!(paths.global_agents_file().starts_with(&paths.config_dir));
        assert!(
            paths
                .global_hook_settings_file()
                .starts_with(&paths.config_dir)
        );
        assert!(
            paths
                .project_config_file()
                .unwrap()
                .starts_with(paths.project_dir.as_ref().unwrap())
        );
        assert!(paths.prompts_dir().starts_with(&paths.data_dir));
        assert!(paths.themes_dir().starts_with(&paths.data_dir));
        assert!(paths.docs_dir().starts_with(&paths.data_dir));
        assert!(paths.memory_dir().starts_with(&paths.data_dir));
        assert!(
            paths
                .portable_agent_skills_dir()
                .starts_with(&paths.data_dir)
        );
        assert!(paths.learned_skills_db().starts_with(&paths.local_data_dir));
        assert!(paths.embedding_models_dir().starts_with(&paths.cache_dir));
        assert!(
            paths
                .learned_skills_cache_dir()
                .starts_with(&paths.cache_dir)
        );
        assert!(paths.sessions_dir().starts_with(&paths.state_dir));
        assert!(paths.tool_outputs_dir().starts_with(&paths.state_dir));
        assert!(paths.transcripts_dir().starts_with(&paths.state_dir));
        assert!(paths.logs_dir().starts_with(&paths.state_dir));
        assert!(paths.chat_history_file().starts_with(&paths.state_dir));
        assert!(paths.hook_trust_file().starts_with(&paths.state_dir));
        assert!(
            paths
                .effect_audit()
                .directory()
                .starts_with(&paths.state_dir)
        );
        assert!(
            paths
                .project_config_trust_file()
                .starts_with(&paths.state_dir)
        );
        assert!(paths.mcp_oauth_dir().starts_with(&paths.credentials_dir));
        assert!(paths.credentials_dir.starts_with(root.join("credentials")));
    }

    #[test]
    fn persistent_artifact_ownership_config_load_never_falls_back_to_data() {
        let (root, paths) = isolated_paths();
        crate::fs::ensure_private_directory(&paths.config_dir).unwrap();
        crate::fs::ensure_private_directory(&paths.data_dir).unwrap();
        crate::fs::private_atomic_create_sync(
            &paths.config_dir.join("config.toml"),
            b"provider = \"openai\"\n",
        )
        .unwrap();
        crate::fs::private_atomic_create_sync(
            &paths.data_dir.join("config.toml"),
            b"provider = \"anthropic\"\n",
        )
        .unwrap();

        let (config, is_first_startup) = crate::config::load_with_paths(&paths, false);

        assert!(!is_first_startup);
        assert_eq!(config.provider.as_deref(), Some("openai"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_path_migration_is_verified_idempotent_and_retains_source() {
        let (root, paths) = isolated_paths();
        let legacy = paths.data_dir.join("sessions");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("one.json"), b"session").unwrap();
        let request = LegacyMigrationRequest {
            artifact: "sessions",
            canonical: paths.sessions_dir(),
            candidates: vec![legacy.clone()],
            marker: paths.migration_markers_dir().join("sessions.json"),
            requirement: LegacyArtifactRequirement::Optional,
            kind: LegacyArtifactKind::Directory,
            selected: None,
        };

        assert_eq!(
            migrate_legacy_path(&request).unwrap(),
            LegacyMigrationStatus::Migrated {
                source: legacy.clone()
            }
        );
        assert_eq!(
            std::fs::read(paths.sessions_dir().join("one.json")).unwrap(),
            b"session"
        );
        assert!(legacy.join("one.json").is_file());
        assert!(request.marker.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(paths.sessions_dir())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(paths.sessions_dir().join("one.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert_eq!(
            migrate_legacy_path(&request).unwrap(),
            LegacyMigrationStatus::CanonicalPresent
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_path_migration_repairs_marker_after_publish_interruption() {
        let (root, paths) = isolated_paths();
        std::fs::create_dir_all(&root).unwrap();
        let legacy = root.join("legacy");
        let marker_blocker = root.join("marker-parent");
        std::fs::write(&legacy, b"content").unwrap();
        std::fs::write(&marker_blocker, b"not a directory").unwrap();
        let request = LegacyMigrationRequest {
            artifact: "interrupted",
            canonical: paths.state_dir.join("canonical"),
            candidates: vec![legacy.clone()],
            marker: marker_blocker.join("marker.json"),
            requirement: LegacyArtifactRequirement::Required,
            kind: LegacyArtifactKind::File,
            selected: None,
        };

        assert!(matches!(
            migrate_legacy_path(&request),
            Err(LegacyMigrationError::Io { .. })
        ));
        assert_eq!(std::fs::read(&request.canonical).unwrap(), b"content");
        assert_eq!(std::fs::read(&legacy).unwrap(), b"content");

        std::fs::remove_file(&marker_blocker).unwrap();
        assert_eq!(
            migrate_legacy_path(&request).unwrap(),
            LegacyMigrationStatus::CanonicalPresent
        );
        assert!(request.marker.is_file());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_path_migration_never_chooses_differing_candidates() {
        let (root, paths) = isolated_paths();
        let first = root.join("legacy-a");
        let second = root.join("legacy-b");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("value"), b"a").unwrap();
        std::fs::write(second.join("value"), b"b").unwrap();
        let canonical = paths.memory_dir();
        let required = LegacyMigrationRequest {
            artifact: "memory",
            canonical: canonical.clone(),
            candidates: vec![first.clone(), second.clone()],
            marker: paths.migration_markers_dir().join("memory.json"),
            requirement: LegacyArtifactRequirement::Required,
            kind: LegacyArtifactKind::Directory,
            selected: None,
        };

        assert!(matches!(
            migrate_legacy_path(&required),
            Err(LegacyMigrationError::Conflict { .. })
        ));
        assert!(!canonical.exists());

        let optional = LegacyMigrationRequest {
            requirement: LegacyArtifactRequirement::Optional,
            ..required
        };
        assert!(matches!(
            migrate_legacy_path(&optional).unwrap(),
            LegacyMigrationStatus::DisabledByConflict(_)
        ));
        assert!(!canonical.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_path_migration_converges_identical_candidates_deterministically() {
        let (root, paths) = isolated_paths();
        std::fs::create_dir_all(&root).unwrap();
        let first = root.join("legacy-a");
        let second = root.join("legacy-b");
        std::fs::write(&first, b"same").unwrap();
        std::fs::write(&second, b"same").unwrap();
        let request = LegacyMigrationRequest {
            artifact: "identical",
            canonical: paths.state_dir.join("identical"),
            candidates: vec![second, first.clone()],
            marker: paths.migration_markers_dir().join("identical.json"),
            requirement: LegacyArtifactRequirement::Required,
            kind: LegacyArtifactKind::File,
            selected: None,
        };

        assert_eq!(
            migrate_legacy_path(&request).unwrap(),
            LegacyMigrationStatus::Migrated { source: first }
        );
        assert_eq!(std::fs::read(&request.canonical).unwrap(), b"same");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_path_migration_honors_explicit_selection() {
        let (root, paths) = isolated_paths();
        std::fs::create_dir_all(&root).unwrap();
        let first = root.join("legacy-a");
        let second = root.join("legacy-b");
        std::fs::write(&first, b"a").unwrap();
        std::fs::write(&second, b"b").unwrap();
        let canonical = paths.state_dir.join("selected");
        let request = LegacyMigrationRequest {
            artifact: "selection",
            canonical: canonical.clone(),
            candidates: vec![first.clone(), second.clone()],
            marker: paths.migration_markers_dir().join("selection.json"),
            requirement: LegacyArtifactRequirement::Required,
            kind: LegacyArtifactKind::File,
            selected: Some(second.clone()),
        };

        assert_eq!(
            migrate_legacy_path(&request).unwrap(),
            LegacyMigrationStatus::Migrated { source: second }
        );
        assert_eq!(std::fs::read(canonical).unwrap(), b"b");
        assert_eq!(std::fs::read(first).unwrap(), b"a");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn legacy_path_migration_rejects_links_without_creating_canonical_content() {
        use std::os::unix::fs::symlink;

        let (root, paths) = isolated_paths();
        std::fs::create_dir_all(&root).unwrap();
        let target = root.join("target");
        let legacy = root.join("legacy-link");
        std::fs::write(&target, b"secret").unwrap();
        symlink(&target, &legacy).unwrap();
        let canonical = paths.state_dir.join("copied");
        let request = LegacyMigrationRequest {
            artifact: "linked",
            canonical: canonical.clone(),
            candidates: vec![legacy],
            marker: paths.migration_markers_dir().join("linked.json"),
            requirement: LegacyArtifactRequirement::Required,
            kind: LegacyArtifactKind::File,
            selected: None,
        };

        assert!(matches!(
            migrate_legacy_path(&request),
            Err(LegacyMigrationError::Io { .. })
        ));
        assert!(!canonical.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn legacy_path_migration_rejects_symlinked_parent_without_creating_canonical_content() {
        use std::os::unix::fs::symlink;

        let (root, paths) = isolated_paths();
        let outside = root.with_extension("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("legacy"), b"secret").unwrap();
        symlink(&outside, root.join("legacy-root")).unwrap();

        let canonical = paths.state_dir.join("copied");
        let request = LegacyMigrationRequest {
            artifact: "linked parent",
            canonical: canonical.clone(),
            candidates: vec![root.join("legacy-root").join("legacy")],
            marker: paths.migration_markers_dir().join("linked-parent.json"),
            requirement: LegacyArtifactRequirement::Required,
            kind: LegacyArtifactKind::File,
            selected: None,
        };

        let error = migrate_legacy_path(&request).unwrap_err();
        let LegacyMigrationError::Io { source, .. } = error else {
            panic!("expected an I/O error for a symlinked parent");
        };
        assert!(source.to_string().contains(PRIVATE_PATH_LINK_POLICY));
        assert!(!canonical.exists());
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }
}
