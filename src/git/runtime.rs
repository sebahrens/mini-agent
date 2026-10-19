//! Publisher-approved Git executable authority and retained model-write roots.
//!
//! Bundle metadata describes files; only hashes compiled into this binary grant
//! executable authority. This module never discovers a replacement through PATH.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use sha2::{Digest, Sha256};

pub(crate) const POLICY: &str = "mini-agent-executable-scope-v3-env";
const MAX_ROOTS: usize = 256;
const MAX_FILES: usize = 128;
const MAX_FILE_BYTES: usize = 64 * 1024 * 1024;
const MAX_BUNDLE_BYTES: usize = 256 * 1024 * 1024;

pub(crate) struct RuntimeFile {
    pub(crate) relative_path: &'static str,
    pub(crate) sha256: &'static str,
    pub(crate) contents: &'static [u8],
    pub(crate) executable: bool,
}

pub(crate) struct RuntimeApproval {
    pub(crate) id: &'static str,
    pub(crate) target: &'static str,
    pub(crate) policy: &'static str,
    pub(crate) files: &'static [RuntimeFile],
}

include!(concat!(env!("OUT_DIR"), "/git_runtime_approval.rs"));

#[derive(Default)]
struct ModelRoots {
    roots: Vec<Weak<crate::paths::WorkspaceBinding>>,
    runtimes: Vec<Weak<crate::paths::WorkspaceBinding>>,
    programs: Vec<Weak<TrustedProgram>>,
    host_writes: Vec<HostWrites>,
    refused: bool,
}

/// An active host-full command can write every host path except its mounted
/// readonly exceptions. Reuse the Sandbox's retained root as the lifetime
/// anchor; this record never grants an executable or keeps a command alive.
struct HostWrites {
    owner: Weak<Mutex<Vec<Arc<crate::paths::WorkspaceBinding>>>>,
    binding: Weak<crate::paths::WorkspaceBinding>,
    readonly: Vec<(PathBuf, crate::fs::CheckedMetadata)>,
}

pub(crate) fn record_host_writes(
    owner: &Arc<Mutex<Vec<Arc<crate::paths::WorkspaceBinding>>>>,
    binding: &Arc<crate::paths::WorkspaceBinding>,
    paths: &[PathBuf],
) -> Result<(), String> {
    let readonly = paths
        .iter()
        .map(|path| {
            let canonical = path
                .canonicalize()
                .map_err(|_| "host-full readonly authority changed")?;
            let identity = crate::fs::checked_path_metadata(&canonical)
                .map_err(|_| "host-full readonly authority changed")?;
            Ok((canonical, identity))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut state = MODEL_ROOTS
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|_| "guarded Git host-write authority is unavailable")?;
    binding.validate()?;
    state
        .host_writes
        .retain(|grant| grant.owner.strong_count() > 0);
    if state.refused {
        return Err("guarded Git host-write authority changed".into());
    }
    if state.host_writes.iter().any(|grant| {
        grant.owner.ptr_eq(&Arc::downgrade(owner))
            && grant
                .readonly
                .iter()
                .map(|(p, _)| p)
                .eq(readonly.iter().map(|(p, _)| p))
    }) {
        return Ok(());
    }
    if state.host_writes.len() >= MAX_ROOTS {
        return Err("guarded Git host-write count exceeds its bound".into());
    }
    state.host_writes.push(HostWrites {
        owner: Arc::downgrade(owner),
        binding: Arc::downgrade(binding),
        readonly,
    });
    Ok(())
}

fn require_outside_host_writes(state: &ModelRoots, path: &Path) -> Result<(), String> {
    for grant in &state.host_writes {
        if let Some(_owner) = grant.owner.upgrade() {
            grant
                .binding
                .upgrade()
                .ok_or("guarded Git host-write owner changed")?
                .validate()?;
            for (root, identity) in &grant.readonly {
                let current = crate::fs::checked_path_metadata(root)
                    .map_err(|_| "host-full readonly authority changed")?;
                crate::fs::ensure_same_file(root, identity, &current)
                    .map_err(|_| "host-full readonly authority changed")?;
            }
            if !grant.readonly.iter().any(|(root, identity)| {
                path == root || (identity.is_dir() && path.starts_with(root))
            }) {
                return Err(
                    "approved Git executable overlaps an active host-full write domain".into(),
                );
            }
        }
    }
    Ok(())
}

static MODEL_ROOTS: OnceLock<Mutex<ModelRoots>> = OnceLock::new();

/// Existing Sandbox clones retain the actual root through owned process
/// retirement. Weak accounting never keeps a retired Windows directory locked.
#[cfg_attr(not(any(test, windows)), allow(dead_code))]
pub(crate) fn record_model_root(root: &Arc<crate::paths::WorkspaceBinding>) -> Result<(), String> {
    record_model_roots(&[(root, false)])
}

/// [`record_model_root`] for every root of one launch under a single lock
/// acquisition. Each retained runtime and trusted program binding is validated
/// once for the whole batch instead of once per root. A root flagged
/// `validated` was identity-checked by the caller immediately before this
/// call, so its own binding is not validated a second time; the runtime and
/// program checks, disjointness and the refusal latch apply to every root.
pub(crate) fn record_model_roots(
    roots: &[(&Arc<crate::paths::WorkspaceBinding>, bool)],
) -> Result<(), String> {
    let mut state = MODEL_ROOTS
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|_| "guarded Git model-root authority is unavailable")?;
    state.roots.retain(|root| root.strong_count() > 0);
    state.runtimes.retain(|root| root.strong_count() > 0);
    state.programs.retain(|program| program.strong_count() > 0);
    for (root, validated) in roots {
        if (!validated && root.validate().is_err()) || state.refused {
            state.refused = true;
            return Err("guarded Git model-root authority changed".into());
        }
    }
    if roots.is_empty() {
        return Ok(());
    }
    let root_paths = roots
        .iter()
        .map(|(root, _)| root.root().to_path_buf())
        .collect::<Vec<_>>();
    for runtime in state.runtimes.iter().filter_map(Weak::upgrade) {
        runtime.validate()?;
        require_disjoint(runtime.root(), &root_paths)?;
    }
    for program in state.programs.iter().filter_map(Weak::upgrade) {
        program.validate()?;
        require_disjoint(&program.path, &root_paths)?;
        require_disjoint(&program.target, &root_paths)?;
    }
    for (root, _) in roots {
        if state
            .roots
            .iter()
            .filter_map(Weak::upgrade)
            .any(|old| Arc::ptr_eq(&old, root))
        {
            continue;
        }
        if state.roots.len() >= MAX_ROOTS {
            state.refused = true;
            return Err("guarded Git model-root count exceeds its bound".into());
        }
        state.roots.push(Arc::downgrade(root));
    }
    Ok(())
}

pub(crate) fn model_write_roots() -> Result<Vec<PathBuf>, String> {
    let state = MODEL_ROOTS
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|_| "guarded Git model-root authority is unavailable")?;
    if state.refused {
        return Err("guarded Git model-root authority could not be retained".into());
    }
    state
        .roots
        .iter()
        .filter_map(Weak::upgrade)
        .map(|root| {
            root.validate()
                .map_err(|_| "guarded Git model-root authority changed".to_string())?;
            Ok(root.root().to_path_buf())
        })
        .collect()
}

/// Current retained executable authority is a read-only exception to explicit
/// host-wide filesystem writes, never a grant inferred from bundle metadata.
pub(crate) fn protected_runtime_paths() -> Result<Vec<PathBuf>, String> {
    let state = MODEL_ROOTS
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|_| "guarded Git runtime protection is unavailable")?;
    if state.refused {
        return Err("guarded Git runtime protection is unavailable".into());
    }
    let mut paths = Vec::new();
    for root in state.runtimes.iter().filter_map(Weak::upgrade) {
        root.validate()?;
        paths.push(root.root().to_path_buf());
    }
    for program in state.programs.iter().filter_map(Weak::upgrade) {
        program.validate()?;
        paths.push(program.path.clone());
        paths.push(program.target.clone());
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn protect_runtime(root: &Arc<crate::paths::WorkspaceBinding>) -> Result<(), String> {
    let mut state = MODEL_ROOTS
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|_| "guarded Git model-root authority is unavailable")?;
    if state.refused {
        return Err("guarded Git model-root authority is unavailable".into());
    }
    require_outside_host_writes(&state, root.root())?;
    state.runtimes.retain(|root| root.strong_count() > 0);
    for model in state.roots.iter().filter_map(Weak::upgrade) {
        model.validate()?;
        require_disjoint(root.root(), &[model.root().to_path_buf()])?;
    }
    if state.runtimes.len() >= MAX_ROOTS {
        return Err("guarded Git runtime root count exceeds its bound".into());
    }
    state.runtimes.push(Arc::downgrade(root));
    Ok(())
}

/// A startup-resolved shorthand service retains its original executable name
/// (Git uses argv[0] for helper aliases), and its resolved target identity.
pub(super) struct TrustedProgram {
    path: PathBuf,
    target: PathBuf,
    identity: crate::fs::CheckedMetadata,
}
impl TrustedProgram {
    pub(super) fn capture(path: &Path, roots: &[PathBuf]) -> Result<Arc<Self>, String> {
        let target = path
            .canonicalize()
            .map_err(|_| "approved Git service authority is unavailable")?;
        require_disjoint(path, roots)?;
        require_disjoint(&target, roots)?;
        let identity = crate::fs::checked_path_metadata(&target)
            .map_err(|_| "approved Git service identity is unavailable")?;
        require_single_link(&identity)?;
        let program = Arc::new(Self {
            path: path.to_path_buf(),
            target,
            identity,
        });
        let mut state = MODEL_ROOTS
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| "guarded Git model-root authority is unavailable")?;
        Self::capture_in_state(program, roots, &mut state)
    }

    fn capture_in_state(
        program: Arc<Self>,
        roots: &[PathBuf],
        state: &mut ModelRoots,
    ) -> Result<Arc<Self>, String> {
        let path = &program.path;
        require_disjoint(path, roots)?;
        require_disjoint(&program.target, roots)?;
        for old in state.programs.iter().filter_map(Weak::upgrade) {
            old.validate()?;
        }
        for old in state.runtimes.iter().filter_map(Weak::upgrade) {
            old.validate()?;
        }
        if state.refused {
            return Err("guarded Git model-root authority is unavailable".into());
        }
        require_outside_host_writes(state, &program.path)?;
        require_outside_host_writes(state, &program.target)?;
        state.programs.retain(|program| program.strong_count() > 0);
        if state.programs.len() >= MAX_ROOTS {
            return Err("approved Git service count exceeds its bound".into());
        }
        for root in state.roots.iter().filter_map(Weak::upgrade) {
            root.validate()?;
            require_disjoint(path, &[root.root().to_path_buf()])?;
            require_disjoint(&program.target, &[root.root().to_path_buf()])?;
        }
        state.programs.push(Arc::downgrade(&program));
        Ok(program)
    }
    pub(super) fn validate(&self) -> Result<(), String> {
        let target = self
            .path
            .canonicalize()
            .map_err(|_| "approved Git service identity changed")?;
        if target != self.target {
            return Err("approved Git service identity changed".into());
        }
        let current = crate::fs::checked_path_metadata(&target)
            .map_err(|_| "approved Git service identity changed")?;
        require_single_link(&current)?;
        crate::fs::ensure_same_file(&target, &self.identity, &current)
            .map_err(|_| "approved Git service identity changed".into())
    }
}

/// Git-for-Windows' explicitly selected original shell and adjacent runtime
/// libraries remain operator services, separate from the approved Git payload.
#[cfg(windows)]
pub(super) struct TrustedShell {
    directory: Arc<crate::paths::WorkspaceBinding>,
    program: Arc<TrustedProgram>,
}

#[cfg(windows)]
impl TrustedShell {
    pub(super) fn capture(directory: &Path, roots: &[PathBuf]) -> Result<Arc<Self>, String> {
        require_disjoint(directory, roots)?;
        let directory = Arc::new(
            crate::paths::WorkspaceBinding::capture(directory)
                .map_err(|_| "approved Git shell directory cannot be retained")?,
        );
        require_disjoint(directory.root(), roots)?;
        let program = TrustedProgram::capture(&directory.root().join("sh.exe"), roots)?;
        // Protect the entire directory: sh.exe loads adjacent runtime DLLs.
        protect_runtime(&directory)?;
        let shell = Arc::new(Self { directory, program });
        shell.validate(roots)?;
        Ok(shell)
    }

    pub(super) fn directory(&self) -> &Path {
        self.directory.root()
    }

    pub(super) fn validate(&self, roots: &[PathBuf]) -> Result<(), String> {
        self.directory.validate()?;
        require_disjoint(self.directory.root(), roots)?;
        self.program.validate()
    }
}

/// Narrow each parent file-mutation authorization, including permission-free
/// tools and JS unrestricted scopes. Existing retained targets and publication
/// code remain responsible for their filesystem race checks.
pub(crate) fn check_model_write(path: &Path) -> Result<(), String> {
    let state = MODEL_ROOTS
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|_| "guarded Git protected-file authority is unavailable")?;
    if !state.runtimes.iter().any(|root| root.strong_count() > 0)
        && !state
            .programs
            .iter()
            .any(|program| program.strong_count() > 0)
    {
        return Ok(());
    }
    let absolute;
    let path = if path.is_absolute() {
        path
    } else {
        absolute = std::env::current_dir()
            .map_err(|_| "model write target cannot be resolved")?
            .join(path);
        &absolute
    };
    let mut existing = path;
    let mut suffix = PathBuf::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            return Err("model write target cannot be resolved".into());
        };
        suffix = Path::new(name).join(suffix);
        existing = existing
            .parent()
            .ok_or("model write target cannot be resolved")?;
    }
    let resolved = existing
        .canonicalize()
        .map_err(|_| "model write target cannot be resolved")?
        .join(suffix);
    for root in state.runtimes.iter().filter_map(Weak::upgrade) {
        root.validate()?;
        require_disjoint(root.root(), std::slice::from_ref(&resolved))?;
    }
    for program in state.programs.iter().filter_map(Weak::upgrade) {
        program.validate()?;
        require_disjoint(&program.path, std::slice::from_ref(&resolved))?;
        require_disjoint(&program.target, std::slice::from_ref(&resolved))?;
    }
    Ok(())
}

pub(crate) fn require_disjoint(path: &Path, roots: &[PathBuf]) -> Result<(), String> {
    for root in roots {
        if path.starts_with(root) || root.starts_with(path) {
            return Err("guarded Git authority overlaps model-writable storage".into());
        }
        // Physical ancestry also covers case aliases, junctions and bind mounts;
        // path spelling alone is not filesystem authority on supported hosts.
        if ancestry_contains(path, root)? || ancestry_contains(root, path)? {
            return Err("guarded Git authority aliases model-writable storage".into());
        }
    }
    Ok(())
}

fn ancestry_contains(child: &Path, parent: &Path) -> Result<bool, String> {
    let parent = match parent.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err("guarded Git authority ancestry is unavailable".into()),
    };
    let parent_metadata = match crate::fs::checked_path_metadata(&parent) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err("guarded Git authority ancestry is unavailable".into()),
    };
    for path in child.ancestors() {
        let resolved = match path.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err("guarded Git authority ancestry is unavailable".into()),
        };
        match crate::fs::checked_path_metadata(&resolved) {
            Ok(metadata) => {
                if crate::fs::ensure_same_file(path, &parent_metadata, &metadata).is_ok() {
                    return Ok(true);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("guarded Git authority ancestry is unavailable".into()),
        }
    }
    Ok(false)
}

struct PinnedFile {
    relative_path: PathBuf,
    identity: crate::fs::CheckedMetadata,
}

pub(crate) struct GuardedRuntime {
    directory: Arc<crate::paths::WorkspaceBinding>,
    files: Vec<PinnedFile>,
    program: PathBuf,
}

impl GuardedRuntime {
    pub(crate) fn materialize(
        approval: &RuntimeApproval,
        storage: &Path,
        roots: &[PathBuf],
    ) -> Result<Self, String> {
        validate_approval(approval)?;
        require_disjoint(storage, roots)?;
        let existing = storage
            .ancestors()
            .find(|path| path.exists())
            .ok_or("guarded Git runtime has no existing storage ancestor")?;
        let canonical = existing
            .canonicalize()
            .map_err(|_| "guarded Git runtime storage ancestor is unavailable")?;
        let suffix = storage
            .strip_prefix(existing)
            .map_err(|_| "guarded Git runtime storage path is invalid")?;
        require_disjoint(&canonical.join(suffix), roots)?;
        crate::paths::ensure_private_directory(storage)
            .map_err(|_| "guarded Git private runtime directory is unavailable")?;
        let directory = Arc::new(
            crate::paths::WorkspaceBinding::capture(storage)
                .map_err(|_| "guarded Git runtime directory cannot be retained")?,
        );
        directory
            .validate_private_root()
            .map_err(|_| "guarded Git runtime directory is not private")?;
        require_disjoint(directory.root(), roots)?;
        protect_runtime(&directory)?;
        let mut files = Vec::with_capacity(approval.files.len());
        for item in approval.files {
            let relative = Path::new(item.relative_path);
            // Current payload is deliberately flat, including DLLs. Nested
            // directory publication is not implicit executable authority.
            match directory.open_regular_relative(relative) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    directory
                        .create_relative_private_atomic(relative, item.contents)
                        .map_err(|_| "guarded Git runtime file publication failed")?;
                }
                Err(_) => return Err("guarded Git runtime file is not a regular file".into()),
            }
            let file = directory
                .open_regular_relative(relative)
                .map_err(|_| "guarded Git runtime file cannot be opened")?;
            let bytes = read_bounded(&file, MAX_FILE_BYTES)?;
            if hex_digest(&bytes) != item.sha256 {
                return Err("guarded Git runtime file disagrees with publisher approval".into());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(if item.executable {
                    0o500
                } else {
                    0o400
                }))
                .map_err(|_| "guarded Git runtime file mode could not be secured")?;
            }
            crate::fs::validate_private_file(&file)
                .map_err(|_| "guarded Git runtime file is not private")?;
            let identity = crate::fs::checked_file_metadata(&file)
                .map_err(|_| "guarded Git runtime file identity unavailable")?;
            require_single_link(&identity)?;
            files.push(PinnedFile {
                relative_path: relative.to_path_buf(),
                identity,
            });
        }
        let program = directory.root().join(program_name());
        let runtime = Self {
            directory,
            files,
            program,
        };
        runtime.validate(roots)?;
        Ok(runtime)
    }

    pub(crate) fn program(&self) -> &Path {
        &self.program
    }

    pub(crate) fn directory(&self) -> &Path {
        self.directory.root()
    }

    pub(crate) fn validate(&self, roots: &[PathBuf]) -> Result<(), String> {
        self.directory
            .validate_private_root()
            .map_err(|_| "guarded Git runtime directory authority changed")?;
        require_disjoint(self.directory.root(), roots)?;
        for pinned in &self.files {
            let file = self
                .directory
                .open_regular_relative(&pinned.relative_path)
                .map_err(|_| "guarded Git runtime file authority changed")?;
            crate::fs::validate_private_file(&file)
                .map_err(|_| "guarded Git runtime file authority changed")?;
            let current = crate::fs::checked_file_metadata(&file)
                .map_err(|_| "guarded Git runtime file authority changed")?;
            require_single_link(&current)?;
            crate::fs::ensure_same_file(
                &self.directory.root().join(&pinned.relative_path),
                &pinned.identity,
                &current,
            )
            .map_err(|_| "guarded Git runtime file authority changed")?;
        }
        Ok(())
    }
}

fn program_name() -> &'static str {
    if cfg!(windows) { "git.exe" } else { "git" }
}

fn validate_approval(approval: &RuntimeApproval) -> Result<(), String> {
    if approval.policy != POLICY
        || approval.target != env!("MINI_AGENT_BUILD_TARGET")
        || approval.id.len() != 64
        || !approval
            .id
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        || approval.files.is_empty()
        || approval.files.len() > MAX_FILES
    {
        return Err("guarded Git publisher approval is unsupported".into());
    }
    let mut names = std::collections::HashSet::new();
    let mut total = 0usize;
    // The embedded payload is immutable static data, so its digest proof is
    // computed once per process per payload. The containment-relevant proof,
    // the on-disk digest in `materialize`, is never memoized.
    let payload_verified = embedded_payload_verified(approval.files);
    for file in approval.files {
        let mut components = Path::new(file.relative_path).components();
        if !matches!(components.next(), Some(Component::Normal(_)))
            || components.next().is_some()
            || file.relative_path.contains(['/', '\\', ':'])
            || !names.insert(file.relative_path.to_ascii_lowercase())
            || file.contents.len() > MAX_FILE_BYTES
            || (!payload_verified && file.sha256 != hex_digest(file.contents))
        {
            return Err("guarded Git publisher payload is invalid".into());
        }
        total = total
            .checked_add(file.contents.len())
            .ok_or("guarded Git payload is too large")?;
        if total > MAX_BUNDLE_BYTES {
            return Err("guarded Git payload is too large".into());
        }
    }
    for name in ["git", "git-remote-http", "git-remote-https"] {
        let filename = if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.into()
        };
        if !approval
            .files
            .iter()
            .any(|file| file.relative_path == filename && file.executable)
        {
            return Err("guarded Git publisher payload omits a required executable".into());
        }
    }
    if !payload_verified {
        record_embedded_payload(approval.files);
    }
    Ok(())
}

/// Static payload slices whose embedded digests were checked in this process.
static VERIFIED_PAYLOADS: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

fn payload_key(files: &'static [RuntimeFile]) -> (usize, usize) {
    (files.as_ptr() as usize, files.len())
}

fn embedded_payload_verified(files: &'static [RuntimeFile]) -> bool {
    VERIFIED_PAYLOADS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains(&payload_key(files))
}

fn record_embedded_payload(files: &'static [RuntimeFile]) {
    let mut verified = VERIFIED_PAYLOADS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let key = payload_key(files);
    if !verified.contains(&key) {
        verified.push(key);
    }
}

pub(crate) fn read_bounded(file: &std::fs::File, limit: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "guarded Git authority could not be read")?;
    if bytes.len() > limit {
        return Err("guarded Git authority exceeds its size bound".into());
    }
    Ok(bytes)
}

pub(crate) fn require_single_link(metadata: &crate::fs::CheckedMetadata) -> Result<(), String> {
    #[cfg(unix)]
    let single = {
        use std::os::unix::fs::MetadataExt;
        metadata.nlink() == 1
    };
    #[cfg(windows)]
    let single =
        crate::fs::windows_file_link_count(metadata.handle()).is_ok_and(|count| count == 1);
    #[cfg(not(any(unix, windows)))]
    let single = false;
    if !metadata.is_file() || !single {
        return Err("guarded Git authority is not a singly-linked regular file".into());
    }
    Ok(())
}

pub(crate) fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_payload_is_private_disjoint_and_revalidated() {
        let fixture =
            super::super::global_snapshot::SnapshotDirectory::new(&std::env::temp_dir()).unwrap();
        let storage = fixture.path().join("runtime");
        let runtime = GuardedRuntime::materialize(
            bundled_approval().expect("test requires approved bundle"),
            &storage,
            &[],
        )
        .unwrap();
        assert!(runtime.program().is_file());
        assert!(runtime.validate(std::slice::from_ref(&storage)).is_err());
        let root = Arc::new(crate::paths::WorkspaceBinding::capture(fixture.path()).unwrap());
        assert!(
            record_model_root(&root).is_err(),
            "later model grant cannot cover retained runtime"
        );
        // Replacing a payload path cannot inherit its compiled authority.
        let helper = storage.join(if cfg!(windows) {
            "git-remote-http.exe"
        } else {
            "git-remote-http"
        });
        std::fs::remove_file(&helper).unwrap();
        std::fs::write(&helper, b"replacement").unwrap();
        assert!(runtime.validate(&[]).is_err());
    }

    fn capture_local(path: &Path, state: &mut ModelRoots) -> Result<Arc<TrustedProgram>, String> {
        let target = path.canonicalize().map_err(|_| "fixture target")?;
        let program = Arc::new(TrustedProgram {
            path: path.to_path_buf(),
            identity: crate::fs::checked_path_metadata(&target).map_err(|_| "fixture metadata")?,
            target,
        });
        TrustedProgram::capture_in_state(program, &[], state)
    }

    #[test]
    fn profile100_host_full_refuses_late_helpers_and_replaced_readonly_exceptions() {
        let fixture = crate::tests::OwnedTestDirectory::new("profile100-host-authority");
        let workspace = fixture.path().join("workspace");
        let cache = fixture.path().join("publisher-cache");
        let outside = fixture.path().join("later-helper");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&cache).unwrap();
        std::fs::write(&outside, b"inert helper").unwrap();
        let owner = Arc::new(crate::paths::WorkspaceBinding::capture(&workspace).unwrap());
        let lease = Arc::new(Mutex::new(vec![owner.clone()]));
        let mut state = ModelRoots::default();
        state.host_writes.push(HostWrites {
            owner: Arc::downgrade(&lease),
            binding: Arc::downgrade(&owner),
            readonly: vec![(
                cache.clone(),
                crate::fs::checked_path_metadata(&cache).unwrap(),
            )],
        });
        assert!(
            capture_local(&outside, &mut state)
                .err()
                .unwrap()
                .contains("host-full write domain")
        );
        let inside = cache.join("later-helper");
        std::fs::write(&inside, b"inert helper").unwrap();
        let accepted = capture_local(&inside, &mut state).unwrap();
        accepted.validate().unwrap();
        drop(accepted);
        std::fs::rename(&cache, fixture.path().join("retired-cache")).unwrap();
        std::fs::create_dir(&cache).unwrap();
        std::fs::write(&inside, b"substituted helper").unwrap();
        assert!(
            capture_local(&inside, &mut state).is_err(),
            "a readonly spelling cannot authorize a replacement generation"
        );
        drop(lease);
        assert!(
            owner.validate().is_ok(),
            "UI may still retain its workspace after the command lease retires"
        );
        assert!(
            capture_local(&outside, &mut state).is_ok(),
            "retired host-write owner must not impose a permanent process-wide restriction"
        );
    }

    #[test]
    fn profile100_custom_write_root_refuses_late_and_preapproved_helpers() {
        let fixture = crate::tests::OwnedTestDirectory::new("profile100-custom-authority");
        let _env = crate::tests::ScopedProcessEnv::set(&[(
            "ZS_CACHE_DIR",
            Some(fixture.path().join("cache").into_os_string()),
        )]);
        let workspace = fixture.path().join("workspace");
        let writable = fixture.path().join("custom-writable");
        for path in [&workspace, &writable] {
            std::fs::create_dir(path).unwrap();
        }
        let helper = writable.join("helper");
        std::fs::write(&helper, b"inert helper").unwrap();
        let sandbox = crate::sandbox::Sandbox::new(true, "__missing_profile100_backend")
            .with_workspace_binding(Arc::new(
                crate::paths::WorkspaceBinding::capture(&workspace).unwrap(),
            ))
            .with_general_profile(
                crate::sandbox::SandboxProfile::HostRead,
                Vec::new(),
                vec![writable.clone()],
            );
        assert!(sandbox.wrap_command("inert").is_err());
        assert!(model_write_roots().unwrap().contains(&writable));
        assert!(
            TrustedProgram::capture(&helper, &[]).is_err(),
            "late selection in a custom write root must refuse"
        );
        drop(sandbox);
        let approved = TrustedProgram::capture(&helper, &[]).unwrap();
        let sandbox = crate::sandbox::Sandbox::new(true, "__missing_profile100_backend")
            .with_workspace_binding(Arc::new(
                crate::paths::WorkspaceBinding::capture(&workspace).unwrap(),
            ))
            .with_general_profile(
                crate::sandbox::SandboxProfile::HostRead,
                Vec::new(),
                vec![writable],
            );
        assert!(
            sandbox
                .wrap_command("inert")
                .unwrap_err()
                .contains("overlap"),
            "preapproved helpers must not acquire a writable alias"
        );
        approved.validate().unwrap();
    }

    #[test]
    fn embedded_payload_digest_memo_never_blesses_another_payload() {
        let original = bundled_approval().expect("test requires approved bundle");
        validate_approval(original).unwrap();
        assert!(embedded_payload_verified(original.files));
        validate_approval(original).unwrap();
        // A different payload with a forged digest is still hashed and refused.
        let forged: &'static [RuntimeFile] = Box::leak(
            original
                .files
                .iter()
                .map(|file| RuntimeFile {
                    relative_path: file.relative_path,
                    sha256: file.sha256,
                    contents: if file.relative_path.starts_with("git") {
                        b"tampered"
                    } else {
                        file.contents
                    },
                    executable: file.executable,
                })
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );
        let tampered = RuntimeApproval {
            id: original.id,
            target: original.target,
            policy: original.policy,
            files: forged,
        };
        assert!(validate_approval(&tampered).is_err());
        assert!(!embedded_payload_verified(forged));
        // Structural checks still run for a memoized payload.
        let wrong = RuntimeApproval {
            id: original.id,
            target: original.target,
            policy: "attacker",
            files: original.files,
        };
        assert!(validate_approval(&wrong).is_err());
    }

    #[test]
    fn source_approval_and_aliases_cannot_bless_modified_payload() {
        let original = bundled_approval().expect("test requires approved bundle");
        let wrong = RuntimeApproval {
            id: original.id,
            target: original.target,
            policy: "attacker",
            files: original.files,
        };
        let fixture =
            super::super::global_snapshot::SnapshotDirectory::new(&std::env::temp_dir()).unwrap();
        assert!(GuardedRuntime::materialize(&wrong, &fixture.path().join("wrong"), &[]).is_err());
        let source = fixture.path().join("source");
        std::fs::write(&source, b"retained inode").unwrap();
        let alias = fixture.path().join("alias");
        std::fs::hard_link(&source, alias).unwrap();
        let metadata = crate::fs::checked_path_metadata(&source).unwrap();
        assert!(require_single_link(&metadata).is_err());
    }
}

#[cfg(test)]
mod mutation_tests {
    use super::*;
    use rig::tool::Tool;

    #[tokio::test]
    async fn direct_builtin_and_js_writes_cannot_modify_retained_executables() {
        let fixture =
            super::super::global_snapshot::SnapshotDirectory::new(&std::env::temp_dir()).unwrap();
        let runtime = GuardedRuntime::materialize(
            bundled_approval().expect("test requires approved bundle"),
            &fixture.path().join("runtime"),
            &[],
        )
        .unwrap();
        let target = runtime.directory().join("new-model-file");
        let tool = crate::agent::tools::WriteTool::new(None, None, None);
        let error = tool
            .call(crate::agent::tools::WriteArgs {
                path: target.to_str().unwrap().into(),
                content: "untrusted".into(),
                overwrite: false,
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Git authority"));
        assert!(!target.exists());
        for name in ["edit", "notebook_edit"] {
            assert!(
                crate::agent::tools::check_perm_path(
                    &None,
                    &None,
                    name,
                    runtime.program().to_str().unwrap()
                )
                .await
                .is_err()
            );
        }
        let binding = crate::paths::WorkspaceBinding::capture(runtime.directory()).unwrap();
        assert!(
            crate::agent::tools::check_perm_bound_path(
                &None,
                &None,
                "write",
                &binding,
                Path::new("new-bound-model-file")
            )
            .await
            .is_err()
        );
        #[cfg(feature = "js")]
        {
            use crate::extras::js::host::{AllowConfig, AuthorizationDecision};
            let policy = AllowConfig::unrestricted(fixture.path());
            assert!(matches!(
                policy.authorize_write(&target),
                AuthorizationDecision::Denied(_)
            ));
            assert!(matches!(
                policy.authorize_write(&fixture.path().join("ordinary-js-write")),
                AuthorizationDecision::Allowed(_)
            ));
        }
        let ordinary = fixture.path().join("ordinary-write");
        tool.call(crate::agent::tools::WriteArgs {
            path: ordinary.to_str().unwrap().into(),
            content: "ordinary".into(),
            overwrite: false,
        })
        .await
        .unwrap();
        assert_eq!(std::fs::read(&ordinary).unwrap(), b"ordinary");
        runtime.validate(&[]).unwrap();
    }
}
