//! Ordinary-command filesystem profiles. The JS worker launcher does not use
//! these grants. Explicit profile setup prepares only missing empty private
//! mountpoint directories through retained parents; bubblewrap never creates
//! those destinations through a writable host-root mount.

use super::*;

pub(super) struct HostFilesystem {
    masks: Vec<(PathBuf, bool)>,
    #[cfg(unix)]
    mounts: Vec<(Arc<crate::paths::WorkspaceBinding>, bool)>,
    #[cfg(unix)]
    files: Vec<Arc<HomeFile>>,
    protected: Vec<PathBuf>,
    scratch: PathBuf,
}

pub(super) fn private_paths() -> Result<Vec<PathBuf>, String> {
    let paths = crate::paths::process_paths()
        .map_err(|_| "sandbox profile: private application paths are unavailable")?;
    let mut denied = vec![
        paths.config_dir,
        paths.credentials_dir,
        paths.state_dir,
        paths.data_dir,
        paths.local_data_dir,
    ];
    let home =
        crate::paths::process_home_dir().ok_or("sandbox profile: home directory is unavailable")?;
    for relative in [
        ".ssh",
        ".aws",
        ".gnupg",
        ".kube",
        ".azure",
        ".config/gcloud",
        ".config/gh",
    ] {
        denied.push(home.join(relative));
    }
    denied.sort();
    denied.dedup();
    Ok(denied)
}

/// Explicit roots per direction, shared by `sandbox-read-roots`,
/// `sandbox-write-roots` and `sandbox-home-mounts`.
pub(super) const MAX_EXPLICIT_ROOTS: usize = 32;

/// Parse one HOME-relative `sandbox-home-mounts` / `sandbox-home-masks`
/// entry. Mount entries may carry a `:rw` suffix; nothing else is accepted.
/// The entry must name a dot file or dot folder directly under HOME (or a
/// path below one) using plain `/`-separated components.
pub(crate) fn parse_home_entry(
    raw: &str,
    key: &'static str,
    allow_writable: bool,
) -> Result<(PathBuf, bool), String> {
    let (body, writable) = match raw.strip_suffix(":rw") {
        Some(body) if allow_writable => (body, true),
        _ => (raw, false),
    };
    let invalid = |why: &str| Err(format!("{key}: entry `{}` {why}", raw.escape_debug()));
    if body.is_empty() || body.len() > 1024 {
        return invalid("is empty or too long");
    }
    if body
        .chars()
        .any(|character| character.is_control() || matches!(character, '\\' | ':'))
    {
        return invalid(if allow_writable {
            "contains a control character, backslash or colon (only a trailing `:rw` is accepted)"
        } else {
            "contains a control character, backslash or colon"
        });
    }
    if body.starts_with('/') || Path::new(body).is_absolute() {
        return invalid("must be relative to HOME, not absolute");
    }
    let segments = body.split('/').collect::<Vec<_>>();
    if segments
        .iter()
        .any(|segment| segment.is_empty() || matches!(*segment, "." | ".."))
    {
        return invalid("must not contain empty, `.` or `..` components");
    }
    if !segments[0].starts_with('.') {
        return invalid("must start with a dot file or dot folder directly under HOME");
    }
    Ok((PathBuf::from(body), writable))
}

/// Built-in private paths plus the configured `sandbox-home-masks`. The
/// configured entries can only add masks; the built-in list is fixed.
pub(super) fn configured_private_paths(masks: &[String]) -> Result<Vec<PathBuf>, String> {
    let mut denied = private_paths()?;
    if masks.is_empty() {
        return Ok(denied);
    }
    if masks.len() > MAX_EXPLICIT_ROOTS {
        return Err(format!(
            "sandbox-home-masks: at most {MAX_EXPLICIT_ROOTS} entries are supported"
        ));
    }
    let home =
        crate::paths::process_home_dir().ok_or("sandbox profile: home directory is unavailable")?;
    for raw in masks {
        let (relative, _) = parse_home_entry(raw, "sandbox-home-masks", false)?;
        denied.push(home.join(relative));
    }
    denied.sort();
    denied.dedup();
    Ok(denied)
}

pub(super) fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

/// Canonical `sandbox-home-mounts` grants that the selected profile does not
/// already provide: workspace receives read and `:rw` grants, host-read only
/// `:rw` grants, host-full none (every entry is still validated).
#[derive(Debug, Default)]
pub(super) struct HomeGrants {
    pub(super) dir_reads: Vec<PathBuf>,
    pub(super) dir_writes: Vec<PathBuf>,
    pub(super) file_reads: Vec<PathBuf>,
}

pub(super) fn home_grants(sandbox: &Sandbox, private: &[PathBuf]) -> Result<HomeGrants, String> {
    let mut grants = HomeGrants::default();
    if sandbox.home_mounts.is_empty() {
        return Ok(grants);
    }
    if sandbox.home_mounts.len() > MAX_EXPLICIT_ROOTS {
        return Err(format!(
            "sandbox-home-mounts: at most {MAX_EXPLICIT_ROOTS} entries are supported"
        ));
    }
    let home =
        crate::paths::process_home_dir().ok_or("sandbox profile: home directory is unavailable")?;
    let canonical_home = home
        .canonicalize()
        .map_err(|_| "sandbox-home-mounts: the home directory cannot be resolved")?;
    let masked = private
        .iter()
        .flat_map(|path| std::iter::once(path.clone()).chain(seatbelt_resolved_spelling(path)))
        .collect::<Vec<_>>();
    for raw in &sandbox.home_mounts {
        let (relative, writable) = parse_home_entry(raw, "sandbox-home-mounts", true)?;
        let entry = raw.escape_debug().to_string();
        let masked_error = || {
            Err(format!(
                "sandbox-home-mounts: entry `{entry}` names, contains or lies inside a masked private path; built-in and configured masks cannot be mounted"
            ))
        };
        let spelled = home.join(&relative);
        if masked.iter().any(|mask| overlaps(&spelled, mask)) {
            return masked_error();
        }
        let canonical = match spelled.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(entry = %entry, "sandbox-home-mounts: skipping missing path");
                continue;
            }
            Err(_) => {
                return Err(format!(
                    "sandbox-home-mounts: entry `{entry}` cannot be resolved"
                ));
            }
        };
        if !canonical.starts_with(&canonical_home) || canonical == canonical_home {
            return Err(format!(
                "sandbox-home-mounts: entry `{entry}` resolves outside HOME (symlink escape)"
            ));
        }
        if masked.iter().any(|mask| overlaps(&canonical, mask)) {
            return masked_error();
        }
        let metadata = std::fs::metadata(&canonical)
            .map_err(|_| format!("sandbox-home-mounts: entry `{entry}` metadata is unavailable"))?;
        if !metadata.is_dir() && !metadata.is_file() {
            return Err(format!(
                "sandbox-home-mounts: entry `{entry}` must be a regular file or directory"
            ));
        }
        if writable && !metadata.is_dir() {
            // Writable roots are retained and reported to guarded Git as
            // model-writable directories; a lone writable file (for example
            // ~/.gitconfig) cannot be tracked that way.
            return Err(format!(
                "sandbox-home-mounts: `:rw` entry `{entry}` must be a directory"
            ));
        }
        match (sandbox.profile, writable, metadata.is_dir()) {
            (SandboxProfile::HostFull, _, _) | (SandboxProfile::HostRead, false, _) => {}
            (_, true, _) => grants.dir_writes.push(canonical),
            (_, false, true) => grants.dir_reads.push(canonical),
            (_, false, false) => grants.file_reads.push(canonical),
        }
    }
    Ok(grants)
}

/// A read-only regular-file grant from `sandbox-home-mounts`, retained by
/// identity and re-validated before every launch like directory roots.
pub(crate) struct HomeFile {
    path: PathBuf,
    checked: crate::fs::CheckedMetadata,
}

impl std::fmt::Debug for HomeFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HomeFile")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl HomeFile {
    fn capture(canonical: &Path) -> Result<Self, String> {
        let checked = crate::fs::checked_path_metadata(canonical)
            .map_err(|_| "sandbox profile: a home file mount is unavailable or changed")?;
        if !checked.is_file() {
            return Err("sandbox profile: a home file mount is no longer a regular file".into());
        }
        Ok(Self {
            path: canonical.to_path_buf(),
            checked,
        })
    }

    pub(super) fn root(&self) -> &Path {
        &self.path
    }

    pub(super) fn validate(&self) -> Result<(), String> {
        let changed = || "sandbox profile: a home file mount is unavailable or changed".to_string();
        if std::fs::canonicalize(&self.path).map_err(|_| changed())? != self.path {
            return Err(changed());
        }
        let current = crate::fs::checked_path_metadata(&self.path).map_err(|_| changed())?;
        if !current.is_file() {
            return Err(changed());
        }
        crate::fs::ensure_same_file(&self.path, &self.checked, &current).map_err(|_| changed())?;
        let (_, held) = crate::fs::file_identity(self.checked.handle()).map_err(|_| changed())?;
        crate::fs::ensure_same_identity(&self.path, &self.checked.identity(), &held)
            .map_err(|_| changed())
    }

    #[cfg(unix)]
    fn handle(&self) -> &std::fs::File {
        self.checked.handle()
    }
}

/// Explicit roots plus validated home mounts, resolved against `base`.
pub(super) struct CapturedRoots {
    pub(super) reads: Vec<Arc<crate::paths::WorkspaceBinding>>,
    pub(super) writes: Vec<Arc<crate::paths::WorkspaceBinding>>,
    pub(super) files: Vec<Arc<HomeFile>>,
}

pub(super) fn capture_roots(sandbox: &Sandbox, base: &Path) -> Result<CapturedRoots, String> {
    if sandbox.home_mounts.is_empty() {
        return Ok(CapturedRoots {
            reads: roots(&sandbox.read_roots, base)?,
            writes: roots(&sandbox.write_roots, base)?,
            files: Vec::new(),
        });
    }
    let private = configured_private_paths(&sandbox.home_masks)?;
    let home = home_grants(sandbox, &private)?;
    let mut reads = sandbox.read_roots.clone();
    reads.extend(home.dir_reads);
    let mut writes = sandbox.write_roots.clone();
    writes.extend(home.dir_writes);
    if reads.len() + home.file_reads.len() > MAX_EXPLICIT_ROOTS || writes.len() > MAX_EXPLICIT_ROOTS
    {
        return Err(format!(
            "sandbox profile: at most {MAX_EXPLICIT_ROOTS} explicit roots are supported per direction, including sandbox-home-mounts"
        ));
    }
    Ok(CapturedRoots {
        reads: roots(&reads, base)?,
        writes: roots(&writes, base)?,
        files: home
            .file_reads
            .iter()
            .map(|path| HomeFile::capture(path).map(Arc::new))
            .collect::<Result<_, _>>()?,
    })
}

pub(super) fn roots(
    paths: &[PathBuf],
    cwd: &Path,
) -> Result<Vec<Arc<crate::paths::WorkspaceBinding>>, String> {
    if paths.len() > MAX_EXPLICIT_ROOTS {
        return Err("sandbox profile: at most 32 explicit roots are supported".into());
    }
    paths
        .iter()
        .map(|path| {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                cwd.join(path)
            };
            #[cfg(test)]
            super::GENERAL_ROOT_CAPTURES.with(|count| count.set(count.get() + 1));
            let root = crate::paths::WorkspaceBinding::capture(&absolute)
                .map_err(|_| "sandbox profile: an explicit root is unavailable or changed")?;
            if root.root().parent().is_none() {
                return Err(
                    "sandbox profile: explicit filesystem-root grants require host-full instead"
                        .into(),
                );
            }
            Ok(Arc::new(root))
        })
        .collect()
}

pub(super) fn protected_host_paths() -> Result<Vec<PathBuf>, String> {
    protected_host_paths_from(crate::git::runtime::protected_runtime_paths()?)
}

/// [`protected_host_paths`] over a runtime-path set the caller already read.
pub(super) fn protected_host_paths_from(
    mut protected: Vec<PathBuf>,
) -> Result<Vec<PathBuf>, String> {
    let paths = crate::paths::process_paths()
        .map_err(|_| "sandbox profile: private application paths are unavailable")?;
    protected.push(
        paths
            .cache_dir
            .canonicalize()
            .map_err(|_| "sandbox profile unavailable: publisher cache must already exist")?,
    );
    protected.sort();
    protected.dedup();
    Ok(protected)
}

impl HostFilesystem {
    pub(super) fn capture(sandbox: &Sandbox, cwd: &Path, cache: &Path) -> Result<Self, String> {
        // Explicit roots are captured once per base and only re-validated on
        // each launch (same cache the Seatbelt arm uses); `append_roots`
        // still validates every mount before it is bound.
        let roots = sandbox.general_roots(cwd)?;
        let reads = roots.reads.clone();
        let writes = roots.writes.clone();
        #[cfg(unix)]
        let files = roots.files.clone();
        let mut masks = Vec::new();
        for path in configured_private_paths(&sandbox.home_masks)? {
            let canonical = match path.canonicalize() {
                Ok(path) => path,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let candidate = super::seatbelt_resolved_spelling(&path).ok_or(
                        "sandbox profile unavailable: missing private mask cannot be resolved",
                    )?;
                    let writable_ancestor = candidate.starts_with(cwd)
                        || candidate.starts_with(cache)
                        || writes.iter().any(|root| candidate.starts_with(root.root()));
                    if sandbox.profile != SandboxProfile::HostFull && !writable_ancestor {
                        // No granted directory can create this absent path.
                        // A readonly host bind needs no writable mountpoint.
                        continue;
                    }
                    prepare_missing_mask(&path)?;
                    path.canonicalize()
                        .map_err(|_| "sandbox profile unavailable: prepared private mask changed")?
                }
                Err(_) => {
                    return Err(
                        "sandbox profile unavailable: private mask cannot be inspected".into(),
                    );
                }
            };
            if path != canonical {
                return Err("sandbox profile unavailable: private mask spelling contains a symlink; use its canonical path".into());
            }
            if canonical.parent().is_none()
                || cwd.starts_with(&canonical)
                || cache.starts_with(&canonical)
            {
                return Err("sandbox profile unavailable: private mask overlaps a required workspace/cache root".into());
            }
            let metadata = canonical
                .metadata()
                .map_err(|_| "sandbox profile: private mask metadata is unavailable")?;
            if !metadata.is_file() && !metadata.is_dir() {
                return Err(
                    "sandbox profile unavailable: private mask must be a regular file or directory"
                        .into(),
                );
            }
            for candidate in [path, canonical] {
                if !masks.iter().any(|(old, _)| *old == candidate) {
                    masks.push((candidate, metadata.is_dir()));
                }
            }
        }
        // Mount containing directories first, before more-specific masks.
        masks.sort_by_key(|(path, _)| path.components().count());
        let mut covered: Vec<(PathBuf, bool)> = Vec::new();
        for (path, directory) in masks {
            if !covered
                .iter()
                .any(|(parent, is_directory)| *is_directory && path.starts_with(parent))
            {
                covered.push((path, directory));
            }
        }
        // Protect the publisher's storage parent as well as current images:
        // an already-running host-full child must not gain writes to a later
        // retained Git generation. Only command scratch is carved back out.
        let protected = protected_host_paths()?;
        if protected
            .iter()
            .any(|path| covered.iter().any(|(mask, _)| path.starts_with(mask)))
        {
            return Err(
                "sandbox profile unavailable: private mask overlaps a retained Git executable"
                    .into(),
            );
        }
        let writable = std::iter::once(cwd)
            .chain(std::iter::once(cache))
            .chain(writes.iter().map(|root| root.root()))
            .collect::<Vec<_>>();
        let mut mounts = reads
            .iter()
            .map(|root| (root.clone(), false))
            .chain(writes.iter().map(|root| (root.clone(), true)))
            .collect::<Vec<_>>();
        // A readonly leaf alone does not stop renaming a writable ancestor.
        // Pin those ancestors as mountpoints without removing ordinary host
        // writes. Sort with the existing grants so a broad ancestor mount
        // cannot hide a narrower grant. Fresh /tmp is already a mountpoint.
        for target in covered.iter().map(|(path, _)| path).chain(protected.iter()) {
            for parent in target
                .ancestors()
                .skip(1)
                .filter(|path| path.parent().is_some())
            {
                if parent == Path::new("/tmp") {
                    continue;
                }
                if parent.starts_with("/proc")
                    || parent.starts_with("/dev")
                    || parent.starts_with("/sys")
                {
                    return Err("sandbox profile unavailable: private/runtime path overlaps a control namespace".into());
                }
                if sandbox.profile == SandboxProfile::HostFull
                    || writable.iter().any(|root| parent.starts_with(root))
                {
                    mounts.push((
                        Arc::new(crate::paths::WorkspaceBinding::capture(parent).map_err(
                            |_| "sandbox profile: protected ancestor cannot be retained",
                        )?),
                        true,
                    ));
                }
            }
        }
        mounts.push((
            Arc::new(
                crate::paths::WorkspaceBinding::capture(cwd)
                    .map_err(|_| "sandbox profile: workspace cannot be retained")?,
            ),
            true,
        ));
        mounts.push((
            Arc::new(
                crate::paths::WorkspaceBinding::capture(cache)
                    .map_err(|_| "sandbox profile: command cache cannot be retained")?,
            ),
            true,
        ));
        mounts.sort_by_key(|(root, write)| {
            (
                root.root().components().count(),
                root.root().to_path_buf(),
                *write,
            )
        });
        mounts.dedup_by(|left, right| left.0.root() == right.0.root() && left.1 == right.1);
        if mounts.len() > 256 {
            return Err("sandbox profile: retained mount count exceeds its bound".into());
        }
        Ok(Self {
            masks: covered,
            #[cfg(unix)]
            mounts,
            #[cfg(unix)]
            files,
            protected,
            scratch: cache.to_path_buf(),
        })
    }

    #[cfg(unix)]
    #[allow(unsafe_code)]
    pub(super) fn append_roots(&self, command: &mut Command) -> Result<(), String> {
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::process::CommandExt;
        let mut retained = Vec::new();
        for (root, write) in &self.mounts {
            root.validate()?;
            let source = root
                .try_clone_directory_file()
                .map_err(|_| "sandbox profile: root handle is unavailable")?;
            // SAFETY: allocate a new owned high descriptor rather than
            // selecting a number that could collide with a Git metadata bind.
            // Parent descriptors stay CLOEXEC until the exact child spawn.
            let fd = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 512) };
            if fd == -1 {
                return Err(
                    "sandbox profile unavailable: cannot retain explicit root descriptors".into(),
                );
            }
            // SAFETY: fcntl returned one new descriptor owned by this File.
            let file = unsafe { std::fs::File::from_raw_fd(fd) };
            command
                .arg(if *write { "--bind-fd" } else { "--ro-bind-fd" })
                .arg(fd.to_string())
                .arg(root.root());
            retained.push(file);
        }
        // Home file mounts are leaves: bind them after every directory grant
        // so no broader directory mount can cover them. Masks follow later.
        for home_file in &self.files {
            home_file.validate()?;
            // SAFETY: as above, a new owned high CLOEXEC descriptor.
            let fd =
                unsafe { libc::fcntl(home_file.handle().as_raw_fd(), libc::F_DUPFD_CLOEXEC, 512) };
            if fd == -1 {
                return Err(
                    "sandbox profile unavailable: cannot retain home file descriptors".into(),
                );
            }
            // SAFETY: fcntl returned one new descriptor owned by this File.
            let file = unsafe { std::fs::File::from_raw_fd(fd) };
            command
                .arg("--ro-bind-fd")
                .arg(fd.to_string())
                .arg(home_file.root());
            retained.push(file);
        }
        // SAFETY: the child closure performs only descriptor syscalls on
        // retained Files; no allocation, filesystem lookup or mutex access.
        unsafe {
            command.as_std_mut().pre_exec(move || {
                for file in &retained {
                    let fd = file.as_raw_fd();
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
        Ok(())
    }

    #[cfg(not(unix))]
    pub(super) fn append_roots(&self, _command: &mut Command) -> Result<(), String> {
        Err("sandbox profiles with explicit roots require a supported Unix backend".into())
    }

    pub(super) fn append_denies(&self, command: &mut Command) {
        for path in &self.protected {
            command.arg("--ro-bind").arg(path).arg(path);
        }
        command.arg("--bind").arg(&self.scratch).arg(&self.scratch);
        for (path, directory) in &self.masks {
            if *directory {
                command
                    .arg("--tmpfs")
                    .arg(path)
                    .arg("--remount-ro")
                    .arg(path);
            } else {
                command.arg("--ro-bind").arg("/dev/null").arg(path);
            }
        }
    }
}

#[cfg(unix)]
fn prepare_missing_mask(path: &Path) -> Result<(), String> {
    use cap_std::fs::DirBuilderExt;
    let mut ancestor = path;
    let mut missing = Vec::new();
    loop {
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if missing.len() >= 64 {
                    return Err("sandbox profile: mask path exceeds component bound".into());
                }
                missing.push(
                    ancestor
                        .file_name()
                        .ok_or("sandbox profile: invalid mask component")?
                        .to_os_string(),
                );
                ancestor = ancestor
                    .parent()
                    .ok_or("sandbox profile: mask parent is unavailable")?;
            }
            Err(_) => return Err("sandbox profile: mask parent cannot be inspected".into()),
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    let binding = crate::paths::WorkspaceBinding::capture(ancestor)
        .map_err(|_| "sandbox profile: mask parent could not be retained")?;
    let mut parent = cap_std::fs::Dir::from_std_file(
        binding
            .try_clone_directory_file()
            .map_err(|_| "sandbox profile: mask parent handle is unavailable")?,
    );
    let mut relative = PathBuf::new();
    for component in missing.iter().rev() {
        binding.validate()?;
        let mut builder = cap_std::fs::DirBuilder::new();
        builder.mode(0o700);
        let created = match parent.create_dir_with(component, &builder) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(_) => return Err("sandbox profile: cannot create a private mask mountpoint".into()),
        };
        relative.push(component);
        // This retained-root traversal refuses links at every new component.
        parent = binding
            .open_dir_relative(&relative)
            .map_err(|_| "sandbox profile: mask component changed or became a link")?;
        if created {
            let file = parent
                .try_clone()
                .map_err(|_| "sandbox profile: mask handle is unavailable")?
                .into_std_file();
            crate::fs::validate_private_directory(&file)
                .map_err(|_| "sandbox profile: new mask is not owned and private")?;
            if parent
                .entries()
                .map_err(|_| "sandbox profile: new mask cannot be inspected")?
                .next()
                .is_some()
            {
                return Err("sandbox profile: newly created mask is no longer empty".into());
            }
        }
        binding.validate()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn prepare_missing_mask(_path: &Path) -> Result<(), String> {
    Err("sandbox profile mask setup requires a supported Unix backend".into())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    struct Fixture {
        root: crate::tests::OwnedTestDirectory,
        _env: crate::tests::ScopedProcessEnv,
        cwd: PathBuf,
        cache: PathBuf,
        home: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root = crate::tests::OwnedTestDirectory::new("profile100");
            let home = root.path().join("home");
            let cwd = root.path().join("workspace");
            let cache = root.path().join("cache/scratch");
            for path in [&home, &cwd, &cache] {
                std::fs::create_dir_all(path).unwrap();
            }
            let env = crate::tests::ScopedProcessEnv::set(&[
                ("HOME", Some(home.clone().into_os_string())),
                (
                    "ZS_CONFIG_DIR",
                    Some(root.path().join("config").into_os_string()),
                ),
                (
                    "ZS_DATA_DIR",
                    Some(root.path().join("data").into_os_string()),
                ),
                (
                    "ZS_STATE_DIR",
                    Some(root.path().join("state").into_os_string()),
                ),
                (
                    "ZS_CACHE_DIR",
                    Some(root.path().join("cache").into_os_string()),
                ),
            ]);
            Self {
                root,
                _env: env,
                cwd,
                cache,
                home,
            }
        }
        fn sandbox(&self, profile: SandboxProfile) -> Sandbox {
            Sandbox::new(true, "bwrap").with_general_profile(profile, Vec::new(), Vec::new())
        }
        fn home_sandbox(
            &self,
            profile: SandboxProfile,
            mounts: &[&str],
            masks: &[&str],
        ) -> Sandbox {
            self.sandbox(profile).with_home_paths(
                mounts.iter().map(|entry| entry.to_string()).collect(),
                masks.iter().map(|entry| entry.to_string()).collect(),
            )
        }
    }

    fn bwrap_args(sandbox: &Sandbox, cwd: &Path, cache: &Path) -> Vec<String> {
        sandbox
            .build_bwrap_command(
                Path::new("/usr/bin/bwrap"),
                "inert",
                cwd,
                cache,
                git_metadata::BwrapGitMetadataBinds::none(),
            )
            .unwrap()
            .as_std()
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn home24w5s_entry_syntax_is_relative_dot_paths_only() {
        let parse = |raw| parse_home_entry(raw, "sandbox-home-mounts", true);
        assert_eq!(
            parse(".gitconfig").unwrap(),
            (PathBuf::from(".gitconfig"), false)
        );
        assert_eq!(parse(".cargo:rw").unwrap(), (PathBuf::from(".cargo"), true));
        assert_eq!(
            parse(".config/git").unwrap(),
            (PathBuf::from(".config/git"), false)
        );
        for invalid in [
            "",
            ":rw",
            "/home/user/.gitconfig",
            "gitconfig",
            "projects/.env",
            ".config/../.ssh",
            "..",
            ".",
            "./.gitconfig",
            ".config//git",
            ".config/",
            ".cargo:ro",
            ".a\\b",
            ".a\nb",
        ] {
            assert!(parse(invalid).is_err(), "`{invalid}` must be rejected");
        }
        // Masks have no writable form.
        assert!(parse_home_entry(".netrc:rw", "sandbox-home-masks", false).is_err());
        assert!(parse_home_entry(".netrc", "sandbox-home-masks", false).is_ok());
    }

    #[test]
    fn home24w5s_mounts_validate_masks_escape_cap_and_missing_paths() {
        let fixture = Fixture::new();
        std::fs::write(fixture.home.join(".gitconfig"), b"[user]\n").unwrap();
        std::fs::create_dir_all(fixture.home.join(".cargo")).unwrap();
        std::fs::create_dir_all(fixture.home.join(".config/gh")).unwrap();
        std::fs::create_dir_all(fixture.home.join(".vault")).unwrap();
        let outside = fixture.root.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, fixture.home.join(".escape")).unwrap();
        std::os::unix::fs::symlink(fixture.home.join(".ssh"), fixture.home.join(".alias")).unwrap();
        std::fs::create_dir(fixture.home.join(".ssh")).unwrap();
        let grants = |profile, mounts: &[&str], masks: &[&str]| {
            let sandbox = fixture.home_sandbox(profile, mounts, masks);
            let private = configured_private_paths(&sandbox.home_masks)?;
            home_grants(&sandbox, &private)
        };
        let canonical_home = fixture.home.canonicalize().unwrap();
        let ok = grants(
            SandboxProfile::Workspace,
            &[".gitconfig", ".cargo:rw", ".npmrc"],
            &[],
        )
        .unwrap();
        assert_eq!(ok.file_reads, [canonical_home.join(".gitconfig")]);
        assert_eq!(ok.dir_writes, [canonical_home.join(".cargo")]);
        assert!(
            ok.dir_reads.is_empty(),
            "missing .npmrc is skipped, not created"
        );
        assert!(!fixture.home.join(".npmrc").exists());

        for (mounts, masks, needle) in [
            (&[".ssh"][..], &[][..], "masked"),
            (&[".ssh/config"][..], &[][..], "masked"),
            (&[".config/gh"][..], &[][..], "masked"),
            (&[".config"][..], &[][..], "masked"),
            (&[".vault"][..], &[".vault"][..], "masked"),
            (&[".alias"][..], &[][..], "masked"),
            (&[".escape"][..], &[][..], "outside HOME"),
            (&[".gitconfig:rw"][..], &[][..], "must be a directory"),
            (&["/etc/passwd"][..], &[][..], "absolute"),
            (&[".config/../.ssh"][..], &[][..], "components"),
            (&["Documents"][..], &[][..], "dot"),
        ] {
            let error = grants(SandboxProfile::Workspace, mounts, masks).unwrap_err();
            assert!(error.contains(needle), "{mounts:?}: {error}");
        }
        let many = (0..33)
            .map(|index| format!(".m{index}"))
            .collect::<Vec<_>>();
        let many = many.iter().map(String::as_str).collect::<Vec<_>>();
        assert!(grants(SandboxProfile::Workspace, &many, &[]).is_err());
        // Home mounts count toward the existing 32-root cap.
        let explicit = (0..31)
            .map(|index| {
                let path = fixture.root.path().join(format!("root{index}"));
                std::fs::create_dir(&path).unwrap();
                path
            })
            .collect::<Vec<_>>();
        let capped = fixture
            .sandbox(SandboxProfile::Workspace)
            .with_general_profile(SandboxProfile::Workspace, explicit, Vec::new())
            .with_home_paths(vec![".gitconfig".into(), ".cargo".into()], Vec::new());
        assert!(
            capture_roots(&capped, &fixture.cwd)
                .err()
                .unwrap()
                .contains("32")
        );
        // Host profiles already read HOME; only writes are added by host-read.
        let host_read =
            grants(SandboxProfile::HostRead, &[".gitconfig", ".cargo:rw"], &[]).unwrap();
        assert!(host_read.file_reads.is_empty() && host_read.dir_reads.is_empty());
        assert_eq!(host_read.dir_writes, [canonical_home.join(".cargo")]);
        let host_full =
            grants(SandboxProfile::HostFull, &[".gitconfig", ".cargo:rw"], &[]).unwrap();
        assert!(host_full.dir_writes.is_empty() && host_full.file_reads.is_empty());
        assert!(grants(SandboxProfile::HostFull, &[".ssh"], &[]).is_err());
    }

    #[test]
    fn home24w5s_bwrap_workspace_mounts_only_configured_home_entries() {
        let fixture = Fixture::new();
        std::fs::write(fixture.home.join(".gitconfig"), b"[user]\n").unwrap();
        std::fs::write(fixture.home.join(".npmrc"), b"token\n").unwrap();
        std::fs::create_dir_all(fixture.home.join(".cargo")).unwrap();
        let canonical_home = fixture.home.canonicalize().unwrap();
        let sandbox =
            fixture.home_sandbox(SandboxProfile::Workspace, &[".gitconfig", ".cargo:rw"], &[]);
        assert!(sandbox.profile_supported());
        let args = bwrap_args(&sandbox, &fixture.cwd, &fixture.cache);
        let bound = |flag: &str, path: &Path| {
            args.windows(3)
                .any(|values| values[0] == flag && Path::new(&values[2]) == path)
        };
        assert!(bound("--ro-bind-fd", &canonical_home.join(".gitconfig")));
        assert!(bound("--bind-fd", &canonical_home.join(".cargo")));
        assert!(
            !args.iter().any(|value| value.contains(".npmrc")),
            "an unconfigured dotfile is never mounted"
        );
        // The file grant follows every directory grant.
        let file = args
            .windows(3)
            .position(|values| Path::new(&values[2]) == canonical_home.join(".gitconfig"))
            .unwrap();
        let cargo = args
            .windows(3)
            .position(|values| Path::new(&values[2]) == canonical_home.join(".cargo"))
            .unwrap();
        assert!(file > cargo);
        // No home configuration keeps the workspace profile's empty root.
        let plain = bwrap_args(
            &fixture.sandbox(SandboxProfile::Workspace),
            &fixture.cwd,
            &fixture.cache,
        );
        assert!(!plain.iter().any(|value| value.contains(".gitconfig")));
    }

    #[cfg(unix)]
    #[test]
    fn bwrap_host_filesystem_captures_explicit_roots_once_and_revalidates_each_launch() {
        let fixture = Fixture::new();
        let read_root = fixture.root.path().join("read-root");
        let write_root = fixture.root.path().join("write-root");
        std::fs::create_dir(&read_root).unwrap();
        std::fs::create_dir(&write_root).unwrap();
        let sandbox = Sandbox::new(true, "bwrap").with_general_profile(
            SandboxProfile::HostRead,
            vec![read_root.clone()],
            vec![write_root.clone()],
        );
        let captures = || crate::sandbox::GENERAL_ROOT_CAPTURES.with(std::cell::Cell::get);
        let before = captures();
        let first = bwrap_args(&sandbox, &fixture.cwd, &fixture.cache);
        assert_eq!(
            captures() - before,
            2,
            "first launch captures each root once"
        );
        let second = bwrap_args(&sandbox, &fixture.cwd, &fixture.cache);
        assert_eq!(captures() - before, 2, "second launch reuses the bindings");
        assert_eq!(first.len(), second.len());
        let canonical = read_root.canonicalize().unwrap();
        assert!(
            second
                .windows(3)
                .any(|v| v[0] == "--ro-bind-fd" && Path::new(&v[2]) == canonical)
        );

        // A replaced root fails identity validation and is captured again.
        std::fs::remove_dir(&read_root).unwrap();
        std::fs::create_dir(&read_root).unwrap();
        let before_replacement = captures();
        bwrap_args(&sandbox, &fixture.cwd, &fixture.cache);
        assert_eq!(captures() - before_replacement, 2);
    }

    #[test]
    fn home24w5s_extra_masks_hide_paths_in_host_read() {
        let fixture = Fixture::new();
        let vault = fixture.home.join(".vault");
        std::fs::create_dir(&vault).unwrap();
        std::fs::write(fixture.home.join(".netrc"), b"machine x").unwrap();
        let sandbox = fixture.home_sandbox(SandboxProfile::HostRead, &[], &[".vault", ".netrc"]);
        let plan = HostFilesystem::capture(&sandbox, &fixture.cwd, &fixture.cache).unwrap();
        assert!(plan.masks.iter().any(|(path, dir)| *path == vault && *dir));
        assert!(
            plan.masks
                .iter()
                .any(|(path, dir)| *path == fixture.home.join(".netrc") && !*dir)
        );
        let args = bwrap_args(&sandbox, &fixture.cwd, &fixture.cache);
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--tmpfs" && Path::new(&pair[1]) == vault)
        );
        // Built-in masks remain regardless of the configured list.
        let ssh = fixture.home.join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        let plan = HostFilesystem::capture(&sandbox, &fixture.cwd, &fixture.cache).unwrap();
        assert!(plan.masks.iter().any(|(path, _)| *path == ssh));
        assert!(
            fixture
                .home_sandbox(SandboxProfile::HostRead, &[], &["/abs"])
                .expanded_filesystem(&fixture.cwd, &fixture.cache)
                .is_err(),
            "an invalid mask fails closed at capture"
        );
    }

    #[test]
    fn home24w5s_seatbelt_profile_reads_mounts_and_denies_extra_masks() {
        let fixture = Fixture::new();
        std::fs::write(fixture.home.join(".gitconfig"), b"[user]\n").unwrap();
        std::fs::create_dir(fixture.home.join(".vault")).unwrap();
        let canonical_home = fixture.home.canonicalize().unwrap();
        let literal = |path: &Path| super::super::seatbelt_string_literal(path, "test").unwrap();
        let sandbox = Sandbox::new(true, "seatbelt").with_home_paths(
            vec![".gitconfig".into(), ".cargo:rw".into()],
            vec![".vault".into()],
        );
        let profile = sandbox
            .get_seatbelt_profile(&fixture.cwd, &fixture.cache, true)
            .unwrap();
        assert!(profile.contains(&format!(
            "(subpath \"{}\")",
            literal(&canonical_home.join(".gitconfig"))
        )));
        assert!(profile.contains(&format!(
            "(subpath \"{}\")",
            literal(&canonical_home.join(".vault"))
        )));
        assert!(!profile.contains(".npmrc"));
        let plain = Sandbox::new(true, "seatbelt")
            .get_seatbelt_profile(&fixture.cwd, &fixture.cache, true)
            .unwrap();
        assert!(!plain.contains(".gitconfig") && !plain.contains(".vault"));
    }

    #[test]
    fn home24w5s_appcontainer_refuses_mounts_and_unenforceable_masks() {
        let fixture = Fixture::new();
        let appcontainer = || Sandbox::new(true, "appcontainer");
        assert!(appcontainer().profile_supported());
        assert!(
            !appcontainer()
                .with_home_paths(vec![".gitconfig".into()], Vec::new())
                .profile_supported()
        );
        let masked = appcontainer().with_home_paths(Vec::new(), vec![".vault".into()]);
        assert!(masked.profile_supported(), "nothing from HOME is granted");
        let widened = masked
            .clone()
            .with_windows_appcontainer_roots(vec![fixture.home.clone()], Vec::new());
        assert!(!widened.profile_supported());
        assert!(
            widened
                .unavailable_diagnostic()
                .unwrap()
                .contains("sandbox-home-masks")
        );
        let inside = masked
            .with_windows_appcontainer_roots(vec![fixture.home.join(".vault/sub")], Vec::new());
        assert!(!inside.profile_supported());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    #[ignore = "requires the actual ordinary-command OS sandbox"]
    async fn home24w5s_native_home_mounts_and_masks_are_enforced() {
        let fixture = Fixture::new();
        std::fs::write(fixture.home.join(".gitconfig"), b"configured").unwrap();
        std::fs::write(fixture.home.join(".npmrc"), b"unconfigured").unwrap();
        std::fs::create_dir(fixture.home.join(".cargo")).unwrap();
        std::fs::create_dir(fixture.home.join(".vault")).unwrap();
        std::fs::write(fixture.home.join(".vault/secret"), b"hidden").unwrap();
        std::fs::write(fixture.home.join(".bashrc"), b"ordinary").unwrap();
        let backend = if cfg!(target_os = "linux") {
            "bwrap"
        } else {
            "seatbelt"
        };
        let quote = |path: &Path| format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"));
        let home = fixture.home.canonicalize().unwrap();
        let run = |profile: SandboxProfile, script: String| {
            // The fixture HOME lives under the test temp directory. Linux host
            // profiles mount a fresh private /tmp over the host root, so a HOME
            // there is only visible through a retained root. Re-expose it as
            // an explicit read root, as the profile100 native test does; real
            // HOME directories are not under /tmp. Workspace must not get it:
            // it would expose the unconfigured ~/.npmrc.
            let reads = if cfg!(target_os = "linux") && profile == SandboxProfile::HostRead {
                vec![fixture.home.clone()]
            } else {
                Vec::new()
            };
            let sandbox = Sandbox::new(true, backend)
                .with_workspace_binding(Arc::new(
                    crate::paths::WorkspaceBinding::capture(&fixture.cwd).unwrap(),
                ))
                .with_general_profile(profile, reads, Vec::new())
                .with_home_paths(
                    vec![".gitconfig".into(), ".cargo:rw".into()],
                    vec![".vault".into()],
                );
            assert_eq!(sandbox.policy(), SandboxPolicy::RequiredAndAvailable);
            let command = sandbox.wrap_command(&script).unwrap();
            async move {
                Sandbox::new(false, "none")
                    .output_built_command_with_limits(command, DEFAULT_COMMAND_LIMITS)
                    .await
                    .unwrap()
            }
        };
        let workspace = format!(
            "set -e; test \"$(cat {git})\" = configured; if cat {npm} >/dev/null 2>&1; then exit 13; fi; printf cargo > {cargo}/written; if printf x > {git} 2>/dev/null; then exit 14; fi; if cat {secret} >/dev/null 2>&1; then exit 15; fi; printf HOME24W5S_WORKSPACE",
            git = quote(&home.join(".gitconfig")),
            npm = quote(&home.join(".npmrc")),
            cargo = quote(&home.join(".cargo")),
            secret = quote(&home.join(".vault/secret")),
        );
        let output = run(SandboxProfile::Workspace, workspace).await;
        assert!(
            output.exit_status.is_some_and(|status| status.success()),
            "workspace: exit={:?}: {}",
            output.exit_status,
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"HOME24W5S_WORKSPACE");
        assert_eq!(
            std::fs::read(fixture.home.join(".cargo/written")).unwrap(),
            b"cargo"
        );
        assert_eq!(
            std::fs::read(fixture.home.join(".gitconfig")).unwrap(),
            b"configured"
        );
        let host_read = format!(
            "set -e; test \"$(cat {bashrc})\" = ordinary; if cat {secret} >/dev/null 2>&1; then exit 13; fi; printf again > {cargo}/host-read; printf HOME24W5S_HOST_READ",
            bashrc = quote(&home.join(".bashrc")),
            secret = quote(&home.join(".vault/secret")),
            cargo = quote(&home.join(".cargo")),
        );
        let output = run(SandboxProfile::HostRead, host_read).await;
        assert!(
            output.exit_status.is_some_and(|status| status.success()),
            "host-read: exit={:?}: {}",
            output.exit_status,
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"HOME24W5S_HOST_READ");
        assert_eq!(
            std::fs::read(fixture.home.join(".vault/secret")).unwrap(),
            b"hidden"
        );
    }

    #[test]
    fn profile100_absent_masks_prepare_only_when_command_can_create_them() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let fixture = Fixture::new();
        let secret = fixture.home.join(".ssh");
        let readonly = HostFilesystem::capture(
            &fixture.sandbox(SandboxProfile::HostRead),
            &fixture.cwd,
            &fixture.cache,
        )
        .unwrap();
        assert!(!secret.exists());
        assert!(!readonly.masks.iter().any(|(path, _)| path == &secret));
        let full = HostFilesystem::capture(
            &fixture.sandbox(SandboxProfile::HostFull),
            &fixture.cwd,
            &fixture.cache,
        )
        .unwrap();
        assert_eq!(std::fs::metadata(&secret).unwrap().mode() & 0o777, 0o700);
        assert_eq!(std::fs::read_dir(&secret).unwrap().count(), 0);
        assert!(full.masks.iter().any(|(path, dir)| path == &secret && *dir));
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o750)).unwrap();
        HostFilesystem::capture(
            &fixture.sandbox(SandboxProfile::HostFull),
            &fixture.cwd,
            &fixture.cache,
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(&secret).unwrap().mode() & 0o777,
            0o750,
            "existing permissions must never be repaired"
        );
        let granted = fixture
            .sandbox(SandboxProfile::HostRead)
            .with_general_profile(
                SandboxProfile::HostRead,
                Vec::new(),
                vec![fixture.home.clone()],
            );
        std::fs::remove_dir(&secret).unwrap();
        HostFilesystem::capture(&granted, &fixture.cwd, &fixture.cache).unwrap();
        assert!(
            secret.is_dir(),
            "host-read write grants need real private mountpoints too"
        );
    }

    #[test]
    fn profile100_mask_setup_refuses_links_and_retains_explicit_root_identity() {
        let fixture = Fixture::new();
        let alias = fixture.home.join(".ssh");
        std::os::unix::fs::symlink(&fixture.cwd, &alias).unwrap();
        assert!(
            HostFilesystem::capture(
                &fixture.sandbox(SandboxProfile::HostFull),
                &fixture.cwd,
                &fixture.cache
            )
            .is_err()
        );
        assert!(
            std::fs::symlink_metadata(&alias)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        std::fs::remove_file(&alias).unwrap();
        let extra = fixture.root.path().join("extra");
        std::fs::create_dir(&extra).unwrap();
        let sandbox = fixture
            .sandbox(SandboxProfile::HostRead)
            .with_general_profile(SandboxProfile::HostRead, Vec::new(), vec![extra.clone()]);
        let plan = HostFilesystem::capture(&sandbox, &fixture.cwd, &fixture.cache).unwrap();
        std::fs::rename(&extra, fixture.root.path().join("retired-extra")).unwrap();
        std::fs::create_dir(&extra).unwrap();
        let mut command = fixture
            .sandbox(SandboxProfile::Workspace)
            .build_bwrap_command(
                Path::new("/usr/bin/bwrap"),
                "inert",
                &fixture.cwd,
                &fixture.cache,
                git_metadata::BwrapGitMetadataBinds::none(),
            )
            .unwrap();
        assert!(
            plan.append_roots(&mut command).is_err(),
            "changed root must not be published"
        );
        assert!(roots(&vec![fixture.cwd.clone(); 33], &fixture.cwd).is_err());
        assert!(roots(&[PathBuf::from("/")], &fixture.cwd).is_err());
    }

    #[test]
    fn profile100_host_mounts_pin_private_ancestors_and_restore_narrow_grants() {
        let fixture = Fixture::new();
        let read = fixture.root.path().join("read");
        let write = fixture.root.path().join("write");
        std::fs::create_dir(&read).unwrap();
        std::fs::create_dir(&write).unwrap();
        let sandbox = fixture
            .sandbox(SandboxProfile::HostFull)
            .with_general_profile(
                SandboxProfile::HostFull,
                vec![read.clone()],
                vec![write.clone()],
            );
        let plan = HostFilesystem::capture(&sandbox, &fixture.cwd, &fixture.cache).unwrap();
        assert!(
            plan.mounts
                .iter()
                .any(|(root, writable)| root.root() == fixture.home && *writable)
        );
        let depths = plan
            .mounts
            .iter()
            .map(|(root, _)| root.root().components().count())
            .collect::<Vec<_>>();
        assert!(depths.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(plan.protected.contains(&fixture.root.path().join("cache")));
        assert!(
            plan.mounts
                .iter()
                .any(|(root, writable)| root.root() == write && *writable)
        );
        assert!(
            plan.mounts
                .iter()
                .any(|(root, writable)| root.root() == read && !*writable)
        );
    }
    #[test]
    fn profile100_actual_host_argv_keeps_root_masks_git_order_and_network_isolation() {
        let fixture = Fixture::new();
        let repository =
            git_metadata::fixtures::plain_repository(fixture.root.path(), "repository");
        let secret = fixture.home.join(".ssh");
        std::fs::create_dir(&secret).unwrap();
        for profile in [SandboxProfile::HostRead, SandboxProfile::HostFull] {
            let sandbox = fixture.sandbox(profile).with_workspace_binding(Arc::new(
                crate::paths::WorkspaceBinding::capture(&repository).unwrap(),
            ));
            let metadata = bwrap_git_metadata_binds_with_capability(&repository, true).unwrap();
            let command = sandbox
                .build_bwrap_command(
                    Path::new("/usr/bin/bwrap"),
                    "inert",
                    &repository,
                    &fixture.cache,
                    metadata,
                )
                .unwrap();
            let args = command
                .as_std()
                .get_args()
                .map(|value| value.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let first_bind = args
                .iter()
                .position(|value| {
                    matches!(
                        value.as_str(),
                        "--bind" | "--ro-bind" | "--bind-fd" | "--ro-bind-fd"
                    )
                })
                .unwrap();
            assert_eq!(
                &args[first_bind..first_bind + 3],
                &[
                    if profile == SandboxProfile::HostFull {
                        "--bind"
                    } else {
                        "--ro-bind"
                    },
                    "/",
                    "/"
                ]
            );
            for pair in [["--proc", "/proc"], ["--dev", "/dev"], ["--tmpfs", "/tmp"]] {
                assert!(args.windows(2).any(|actual| actual == pair));
            }
            assert!(args.iter().any(|value| value == "--unshare-net"));
            assert!(
                args.windows(2)
                    .any(|pair| pair[0] == "--tmpfs" && Path::new(&pair[1]) == secret)
            );
            let workspace = args
                .windows(3)
                .rposition(|values| values[0] == "--bind-fd" && Path::new(&values[2]) == repository)
                .unwrap();
            let config = args
                .windows(3)
                .position(|values| {
                    values[0] == "--ro-bind-fd"
                        && Path::new(&values[2]) == repository.join(".git/config")
                })
                .unwrap();
            assert!(
                config > workspace,
                "Git masks must follow every writable workspace/ancestor grant"
            );
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    #[ignore = "requires the actual ordinary-command OS sandbox"]
    async fn profile100_native_host_profiles_preserve_masks_and_real_operations() {
        let fixture = Fixture::new();
        let secret = fixture.home.join(".ssh");
        std::fs::create_dir(&secret).unwrap();
        std::fs::write(secret.join("secret"), b"must remain hidden").unwrap();
        std::fs::write(fixture.home.join(".bashrc"), b"ordinary home data").unwrap();
        let extra = fixture.root.path().join("explicit-write");
        std::fs::create_dir(&extra).unwrap();
        let quote = |path: &Path| format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"));
        for profile in [SandboxProfile::HostRead, SandboxProfile::HostFull] {
            let backend = if cfg!(target_os = "linux") {
                "bwrap"
            } else {
                "seatbelt"
            };
            let sandbox = Sandbox::new(true, backend)
                .with_workspace_binding(Arc::new(
                    crate::paths::WorkspaceBinding::capture(&fixture.cwd).unwrap(),
                ))
                .with_general_profile(profile, vec![fixture.home.clone()], vec![extra.clone()]);
            assert_eq!(
                sandbox.policy(),
                SandboxPolicy::RequiredAndAvailable,
                "native backend is required"
            );
            let outside = fixture.root.path().join(format!("outside-{profile}"));
            let outside_check = if profile == SandboxProfile::HostFull {
                format!("printf host > {} || exit 15", quote(&outside))
            } else {
                let readonly_check = format!(
                    "if printf forbidden > {} 2>/dev/null; then exit 15; fi",
                    quote(&fixture.home.join(".bashrc"))
                );
                if cfg!(target_os = "linux") {
                    // /tmp is an intentional private writable tmpfs. This
                    // sibling is scratch, not a writable view of the host.
                    format!(
                        "{readonly_check}; printf private-scratch > {} || exit 16",
                        quote(&outside)
                    )
                } else {
                    format!(
                        "{readonly_check}; if printf forbidden > {} 2>/dev/null; then exit 16; fi",
                        quote(&outside)
                    )
                }
            };
            let fd_check = if cfg!(target_os = "linux") {
                "for f in /proc/self/fd/*; do n=${f##*/}; test \"$n\" -lt 512 || exit 19; done"
            } else {
                ":"
            };
            let script = format!(
                "set -e; test \"$(cat {home}/.bashrc)\" = 'ordinary home data'; printf workspace > {cwd}/written; printf explicit > {extra}/written; if cat {secret}/secret >/dev/null 2>&1; then exit 13; fi; if mv {home} {renamed} 2>/dev/null; then exit 14; fi; {outside_check}; {fd_check}; printf PROFILE100_NATIVE_PASS",
                home = quote(&fixture.home),
                cwd = quote(&fixture.cwd),
                extra = quote(&extra),
                secret = quote(&secret),
                renamed = quote(&fixture.root.path().join("renamed-home"))
            );
            let command = sandbox.wrap_command(&script).unwrap();
            drop(sandbox);
            assert!(
                crate::git::runtime::model_write_roots()
                    .unwrap()
                    .contains(&fixture.cwd),
                "admitted command must retain its write-domain owner after Sandbox retirement"
            );
            let output = Sandbox::new(false, "none")
                .output_built_command_with_limits(command, DEFAULT_COMMAND_LIMITS)
                .await
                .unwrap();
            assert_eq!(output.status, CommandStatus::Completed);
            assert!(
                output.exit_status.is_some_and(|status| status.success()),
                "{profile}: exit={:?}: {}",
                output.exit_status,
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"PROFILE100_NATIVE_PASS");
            assert_eq!(
                std::fs::read(secret.join("secret")).unwrap(),
                b"must remain hidden"
            );
            assert_eq!(
                std::fs::read(fixture.cwd.join("written")).unwrap(),
                b"workspace"
            );
            assert_eq!(std::fs::read(extra.join("written")).unwrap(), b"explicit");
            assert_eq!(
                std::fs::read(fixture.home.join(".bashrc")).unwrap(),
                b"ordinary home data"
            );
            assert_eq!(outside.exists(), profile == SandboxProfile::HostFull);
        }
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires real Linux bubblewrap and an installed HOME rustup toolchain"]
    async fn profile100_native_host_read_runs_cargo_test_from_home_rustup() {
        let host_home = crate::paths::process_home_dir().expect("host HOME");
        let rustup_home = std::env::var_os("RUSTUP_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| host_home.join(".rustup"))
            .canonicalize()
            .expect("installed rustup home");
        let rustup_root = crate::paths::WorkspaceBinding::capture(&rustup_home).unwrap();
        let cargo_shim = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| host_home.join(".cargo"))
            .join("bin/cargo")
            .canonicalize()
            .expect("installed rustup cargo shim");
        let shim_identity = crate::fs::checked_path_metadata(&cargo_shim).unwrap();
        let rustup_program = cargo_shim
            .parent()
            .unwrap()
            .join("rustup")
            .canonicalize()
            .unwrap();
        crate::fs::ensure_same_file(
            &rustup_program,
            &shim_identity,
            &crate::fs::checked_path_metadata(&rustup_program).unwrap(),
        )
        .expect("cargo must be the actual installed rustup proxy (symlink or hardlink)");
        let settings = std::fs::read_to_string(rustup_home.join("settings.toml")).unwrap();
        assert!(settings.len() <= 64 * 1024);
        let settings: toml::Value = toml::from_str(&settings).unwrap();
        let toolchain = settings
            .get("default_toolchain")
            .and_then(toml::Value::as_str)
            .expect("installed default toolchain");
        assert!(toolchain.len() <= 200 && !toolchain.contains(['/', '\\', '\n', '\r']));
        let toolchain_root = crate::paths::WorkspaceBinding::capture(
            &rustup_home.join("toolchains").join(toolchain),
        )
        .unwrap();
        let fixture = Fixture::new();
        std::fs::create_dir_all(fixture.home.join(".cargo/bin")).unwrap();
        // Rustup dispatches by executable basename. Cargo invokes rustc and
        // rustdoc through PATH, so retain the real installed proxy once and
        // copy that same image under all three standard dispatch names.
        let mut proxy = std::fs::File::open(&cargo_shim).unwrap();
        crate::fs::ensure_same_file(
            &cargo_shim,
            &shim_identity,
            &crate::fs::checked_file_metadata(&proxy).unwrap(),
        )
        .unwrap();
        let permissions = proxy.metadata().unwrap().permissions();
        for name in ["cargo", "rustc", "rustdoc"] {
            std::io::Seek::seek(&mut proxy, std::io::SeekFrom::Start(0)).unwrap();
            let mut target = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(fixture.home.join(".cargo/bin").join(name))
                .unwrap();
            std::io::copy(&mut proxy, &mut target).unwrap();
            target.set_permissions(permissions.clone()).unwrap();
        }
        crate::fs::ensure_same_file(
            &cargo_shim,
            &shim_identity,
            &crate::fs::checked_path_metadata(&cargo_shim).unwrap(),
        )
        .unwrap();
        std::os::unix::fs::symlink(&rustup_home, fixture.home.join(".rustup")).unwrap();
        std::fs::create_dir(fixture.cwd.join("src")).unwrap();
        std::fs::write(
            fixture.cwd.join("Cargo.toml"),
            "[package]\nname='profile100-home-rustup'\nversion='0.0.0'\nedition='2024'\n",
        )
        .unwrap();
        std::fs::write(
            fixture.cwd.join("src/lib.rs"),
            "#[test] fn actual_compiled_test() { assert_eq!(2 + 2, 4); }\n",
        )
        .unwrap();
        let linker = std::fs::canonicalize("/usr/bin/cc").unwrap();
        assert!(
            linker.starts_with("/usr/"),
            "owned CI compiler comes from the readonly system runtime"
        );
        let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
        let script = format!(
            "set -e; export HOME={home}; export PATH={bin}:/usr/bin:/bin; export RUSTUP_HOME={rustup}; export RUSTUP_TOOLCHAIN={toolchain}; export CARGO_HOME={cargo_cache}; export RUSTFLAGS={flags}; cargo --version; cargo test --offline --quiet",
            home = quote(fixture.home.to_str().unwrap()),
            bin = quote(fixture.home.join(".cargo/bin").to_str().unwrap()),
            rustup = quote(fixture.home.join(".rustup").to_str().unwrap()),
            toolchain = quote(toolchain),
            cargo_cache = quote(fixture.cwd.join(".cargo-cache").to_str().unwrap()),
            flags = quote(&format!("-Clinker={}", linker.display()))
        );
        let sandbox = fixture
            .sandbox(SandboxProfile::HostRead)
            .with_workspace_binding(Arc::new(
                crate::paths::WorkspaceBinding::capture(&fixture.cwd).unwrap(),
            ))
            .with_general_profile(
                SandboxProfile::HostRead,
                vec![fixture.home.clone()],
                Vec::new(),
            );
        assert_eq!(sandbox.policy(), SandboxPolicy::RequiredAndAvailable);
        let command = sandbox.wrap_command(&script).unwrap();
        let output = sandbox
            .output_built_command_with_limits(command, DEFAULT_COMMAND_LIMITS)
            .await
            .unwrap();
        assert_eq!(output.status, CommandStatus::Completed);
        assert!(
            output.exit_status.is_some_and(|status| status.success()),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            stdout.contains("cargo ") && stdout.contains("test result: ok. 1 passed;"),
            "{stdout}"
        );
        assert!(fixture.cwd.join("target/debug/deps").is_dir());
        assert!(
            !fixture.home.join(".cargo/registry").exists(),
            "Cargo writes are directed to the owned workspace, not the readonly home"
        );
        rustup_root.validate().unwrap();
        toolchain_root.validate().unwrap();
    }
}
