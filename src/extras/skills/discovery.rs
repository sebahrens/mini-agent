//! Captured, bounded instruction-package discovery. Publication is scoped to
//! its original source; a project scan never changes explicitly imported ACTIVE.
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use sha2::{Digest, Sha256};

use crate::paths::{AppPaths, WorkspaceBinding};
use crate::permission::checker::PermCheck;

use super::catalog::AgentSkillCatalog;
use super::import::{SourceTree, publish_tree};
use super::index::AgentSkillRecord;

const MAX_PACKAGES: usize = 64;
const MAX_ENTRIES: usize = 1024;
const MAX_DEPTH: usize = 8;
const MAX_RESOURCE_BYTES: usize = 1024 * 1024;
const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct SourceRoot {
    pub(crate) binding: Arc<WorkspaceBinding>,
    pub(crate) directory: PathBuf,
    pub(crate) origin: &'static str,
    pub(crate) project: bool,
}

#[derive(Clone)]
pub(crate) struct Package {
    pub(crate) record: Arc<AgentSkillRecord>,
    pub(crate) origin: &'static str,
    pub(crate) source: Option<SourceRoot>,
    pub(crate) source_directory: PathBuf,
    pub(crate) source_identity: Option<crate::fs::CheckedMetadata>,
    files: Vec<SourceFile>,
}

#[derive(Clone)]
struct SourceFile {
    path: PathBuf,
    metadata: crate::fs::CheckedMetadata,
    hash: Vec<u8>,
    /// Stat fingerprint recorded while the file's content hash was known to
    /// match. A file whose identity, length, mtime and ctime all still equal it
    /// has not been written since, so the per-call re-read and re-hash is
    /// skipped; any difference falls back to the full hash comparison. Set only
    /// once the timestamps are older than `QUIET_WINDOW_SECS`, so a rewrite in
    /// the same filesystem timestamp tick cannot hide behind it.
    stamp: Arc<OnceLock<FileStamp>>,
    #[cfg(test)]
    full_reads: Arc<std::sync::atomic::AtomicUsize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileStamp {
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

/// Timestamps younger than this may still share a tick with a later write.
const QUIET_WINDOW_SECS: i64 = if cfg!(test) { 0 } else { 3 };

/// The stamp of `metadata`, or `None` when this platform has no ctime to back
/// it or the file changed too recently to be trusted as unchanged.
pub(crate) fn quiet_file_stamp(metadata: &std::fs::Metadata) -> Option<FileStamp> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let stamp = FileStamp {
            len: metadata.len(),
            mtime: (metadata.mtime(), metadata.mtime_nsec()),
            ctime: (metadata.ctime(), metadata.ctime_nsec()),
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs() as i64;
        stamp_is_quiet(&stamp, now).then_some(stamp)
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

fn stamp_is_quiet(stamp: &FileStamp, now_secs: i64) -> bool {
    now_secs.saturating_sub(stamp.mtime.0.max(stamp.ctime.0)) >= QUIET_WINDOW_SECS
}

/// Per-pass memo of the source-level authority decision (workspace validation,
/// project trust, read permission). The decision is identical for every package
/// of one root, so one select evaluates it once per root rather than once per
/// package; the next select starts a fresh memo, so revocation is never cached.
#[derive(Default)]
pub(crate) struct SourceAuthMemo(std::cell::RefCell<Vec<(&'static str, PathBuf, bool)>>);

impl SourceAuthMemo {
    fn authorized(
        &self,
        source: &SourceRoot,
        paths: &AppPaths,
        permission: &Option<PermCheck>,
    ) -> bool {
        if let Some((_, _, verdict)) = self.0.borrow().iter().find(|(origin, directory, _)| {
            *origin == source.origin && *directory == source.directory
        }) {
            return *verdict;
        }
        let verdict = source_authorized(source, paths, permission);
        self.0
            .borrow_mut()
            .push((source.origin, source.directory.clone(), verdict));
        verdict
    }
}

impl Package {
    /// Test seam: how many times `authorized` fell back to reading and hashing
    /// a captured file instead of trusting its unchanged stat fingerprint.
    #[cfg(test)]
    pub(crate) fn full_read_count(&self) -> usize {
        self.files
            .iter()
            .map(|file| file.full_reads.load(std::sync::atomic::Ordering::Relaxed))
            .sum()
    }

    pub(crate) fn authorized(
        &self,
        paths: &AppPaths,
        permission: &Option<PermCheck>,
        memo: &SourceAuthMemo,
    ) -> bool {
        let Some(source) = &self.source else {
            return true;
        };
        if !memo.authorized(source, paths, permission)
            || !allowed(
                &source.binding,
                &self.source_directory.join("SKILL.md"),
                permission,
            )
            || self.record.resources.iter().any(|resource| {
                !allowed(
                    &source.binding,
                    &self.source_directory.join(&resource.relative_path),
                    permission,
                )
            })
        {
            return false;
        }
        let Some(held) = &self.source_identity else {
            return false;
        };
        let Ok(file) = source
            .binding
            .open_relative_directory_file(&self.source_directory)
        else {
            return false;
        };
        let Ok(current) = crate::fs::checked_file_metadata(&file) else {
            return false;
        };
        crate::fs::ensure_same_file(
            &source.binding.root().join(&self.source_directory),
            held,
            &current,
        )
        .is_ok()
            && self.files.iter().all(|captured| {
                let Ok(file) = source.binding.open_regular_relative(&captured.path) else {
                    return false;
                };
                let Ok(metadata) = crate::fs::checked_file_metadata(&file) else {
                    return false;
                };
                if crate::fs::ensure_same_file(
                    &source.binding.root().join(&captured.path),
                    &captured.metadata,
                    &metadata,
                )
                .is_err()
                    || metadata.len() != captured.metadata.len()
                {
                    return false;
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if metadata.nlink() != 1 {
                        return false;
                    }
                }
                #[cfg(windows)]
                if crate::fs::windows_file_link_count(&file).ok() != Some(1) {
                    return false;
                }
                let current = quiet_file_stamp(&metadata);
                if let (Some(recorded), Some(current)) = (captured.stamp.get(), current)
                    && *recorded == current
                {
                    return true;
                }
                #[cfg(test)]
                captured
                    .full_reads
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let mut bytes = Vec::new();
                if file
                    .take(metadata.len().saturating_add(1))
                    .read_to_end(&mut bytes)
                    .is_err()
                    || bytes.len() as u64 != metadata.len()
                {
                    return false;
                }
                let matches = Sha256::digest(&bytes).as_slice() == captured.hash;
                if matches && let Some(current) = current {
                    let _ = captured.stamp.set(current);
                }
                matches
            })
    }
}

#[derive(Default)]
pub(crate) struct Discovery {
    pub(crate) packages: BTreeMap<String, Package>,
    pub(crate) diagnostics: Vec<String>,
    // Catalog owners retain their existing leases for every published record.
    pub(crate) catalogues: Vec<AgentSkillCatalog>,
}

pub(crate) fn allowed(
    binding: &WorkspaceBinding,
    path: &Path,
    permission: &Option<PermCheck>,
) -> bool {
    let Ok(identity) = binding.logical_relative_path(path) else {
        return false;
    };
    permission.as_ref().is_none_or(|checker| {
        checker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .allows_mutation_preimage(&identity.to_string_lossy(), true)
    })
}

pub(crate) fn source_authorized(
    source: &SourceRoot,
    paths: &AppPaths,
    permission: &Option<PermCheck>,
) -> bool {
    source.binding.validate().is_ok()
        && (!source.project
            || crate::config::load::project_config_is_trusted_in_workspace_readonly(
                paths,
                &source.binding,
            ))
        && allowed(&source.binding, &source.directory, permission)
}

/// Stat-only fingerprint of every source root: for each directory and file
/// below it, its relative path, kind, device, inode, length, mtime and ctime
/// (so a rename, a delete, a create and any content write all change it), with
/// directory listings sorted. It reads no file content. `None` means the tree
/// cannot be fingerprinted cheaply and soundly (non-Unix, a stat error, a
/// recently written entry that could share a timestamp tick with a later
/// write, or more entries than discovery itself accepts), and the caller must
/// run the full discovery.
pub(crate) fn source_signature(roots: &[SourceRoot]) -> Option<[u8; 32]> {
    #[cfg(unix)]
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs() as i64;
        let mut hash = Sha256::new();
        let mut budget = MAX_ENTRIES.saturating_mul(4);
        for source in roots {
            hash.update(source.origin.as_bytes());
            hash.update([0]);
            hash.update(source.directory.as_os_str().as_encoded_bytes());
            hash.update([0]);
            let base = source.binding.root().join(&source.directory);
            signature_node(&base, 0, now, &mut budget, &mut hash)?;
        }
        Some(hash.finalize().into())
    }
    #[cfg(not(unix))]
    {
        let _ = roots;
        None
    }
}

#[cfg(unix)]
fn signature_node(
    path: &Path,
    depth: usize,
    now: i64,
    budget: &mut usize,
    hash: &mut Sha256,
) -> Option<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            hash.update(b"absent\0");
            return Some(());
        }
        Err(_) => return None,
    };
    *budget = budget.checked_sub(1)?;
    let kind = metadata.file_type();
    let tag: u8 = if kind.is_symlink() {
        b'l'
    } else if kind.is_dir() {
        b'd'
    } else if kind.is_file() {
        b'f'
    } else {
        b'o'
    };
    let stamp = FileStamp {
        len: metadata.len(),
        mtime: (metadata.mtime(), metadata.mtime_nsec()),
        ctime: (metadata.ctime(), metadata.ctime_nsec()),
    };
    if !stamp_is_quiet(&stamp, now) {
        return None;
    }
    hash.update([tag]);
    hash.update(metadata.dev().to_le_bytes());
    hash.update(metadata.ino().to_le_bytes());
    hash.update(stamp.len.to_le_bytes());
    hash.update(stamp.mtime.0.to_le_bytes());
    hash.update(stamp.mtime.1.to_le_bytes());
    hash.update(stamp.ctime.0.to_le_bytes());
    hash.update(stamp.ctime.1.to_le_bytes());
    if tag == b'd' {
        if depth > MAX_DEPTH + 1 {
            return None;
        }
        let mut names = Vec::new();
        for entry in std::fs::read_dir(path).ok()? {
            names.push(entry.ok()?.file_name());
        }
        names.sort();
        hash.update((names.len() as u64).to_le_bytes());
        for name in names {
            hash.update(name.as_encoded_bytes());
            hash.update([0]);
            signature_node(&path.join(&name), depth + 1, now, budget, hash)?;
        }
    }
    Some(())
}

/// Whether every source root that exists is still authorized. A cached
/// discovery is only reusable when none of its roots lost (or newly gained)
/// the trust and read authority it was built under; a root that cannot even be
/// opened sends the caller to the full pass, which reports why.
pub(crate) fn roots_still_authorized(
    roots: &[SourceRoot],
    paths: &AppPaths,
    permission: &Option<PermCheck>,
) -> bool {
    let memo = SourceAuthMemo::default();
    roots.iter().all(
        |source| match source.binding.open_dir_relative(&source.directory) {
            Ok(_) => memo.authorized(source, paths, permission),
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        },
    )
}

/// Source order is priority order. First valid name wins; invalid packages do
/// not hide valid lower-priority siblings. All IO belongs to the caller's
/// scoped blocking preparation, never a renderer or detached discovery task.
pub(crate) fn discover(
    paths: &AppPaths,
    roots: &[SourceRoot],
    permission: &Option<PermCheck>,
) -> Discovery {
    let mut result = Discovery::default();
    let mut imported = AgentSkillCatalog::new(paths);
    match imported.refresh_lexical() {
        Ok(index) => {
            for record in index.records() {
                if result.packages.len() == MAX_PACKAGES {
                    result
                        .diagnostics
                        .push("skill package limit exceeded".into());
                    break;
                }
                #[cfg(not(feature = "skills"))]
                if !record.learned_js.is_empty() {
                    result
                        .diagnostics
                        .push("imported skill requires the learned 'skills' feature".into());
                    continue;
                }
                result.packages.insert(
                    record.name.clone(),
                    Package {
                        record: Arc::clone(record),
                        origin: "imported",
                        source: None,
                        source_directory: PathBuf::new(),
                        source_identity: None,
                        files: Vec::new(),
                    },
                );
            }
            result.catalogues.push(imported);
        }
        Err(_) => result
            .diagnostics
            .push("imported catalogue unavailable".into()),
    }
    let mut packages = result.packages.len();
    let mut entries = 0usize;
    let mut bytes = 0usize;
    for source in roots {
        let directory = match source.binding.open_dir_relative(&source.directory) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                result.diagnostics.push(format!(
                    "{} skill directory unavailable or linked",
                    source.origin
                ));
                continue;
            }
        };
        if !source_authorized(source, paths, permission) {
            result.diagnostics.push(format!(
                "{} skills require current source trust and read permission",
                source.origin
            ));
            continue;
        }
        let mut names = Vec::new();
        let scan = (|| -> Result<(), String> {
            for entry in directory
                .read_dir(".")
                .map_err(|_| "cannot enumerate source")?
            {
                entries = entries.saturating_add(1);
                if entries > MAX_ENTRIES {
                    return Err("discovery entry limit exceeded".into());
                }
                let entry = entry.map_err(|_| "cannot enumerate source entry")?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| "non-UTF8 skill name")?;
                if !entry
                    .file_type()
                    .map_err(|_| "cannot inspect source entry")?
                    .is_dir()
                {
                    continue;
                }
                names.push(name);
            }
            names.sort();
            Ok(())
        })();
        if let Err(error) = scan {
            result
                .diagnostics
                .push(format!("{}: {error}", source.origin));
            break;
        }
        for name in names {
            packages = packages.saturating_add(1);
            if packages > MAX_PACKAGES {
                result
                    .diagnostics
                    .push("skill package limit exceeded".into());
                return result;
            }
            let package_path = source.directory.join(&name);
            let attempt = (|| -> Result<(), String> {
                let root_file = source
                    .binding
                    .open_relative_directory_file(&package_path)
                    .map_err(|_| "package must be a retained real directory")?;
                let root_identity = crate::fs::checked_file_metadata(&root_file)
                    .map_err(|_| "package identity unavailable")?;
                let mut tree = SourceTree::default();
                let mut files = Vec::new();
                collect(
                    source,
                    permission,
                    &package_path,
                    &package_path,
                    0,
                    &mut entries,
                    &mut bytes,
                    &mut tree,
                    &mut files,
                )?;
                source.binding.validate()?;
                if !source_authorized(source, paths, permission) {
                    return Err("source trust or read permission changed".into());
                }
                let current_root = source
                    .binding
                    .open_relative_directory_file(&package_path)
                    .map_err(|_| "package changed during discovery")?;
                let current_identity = crate::fs::checked_file_metadata(&current_root)
                    .map_err(|_| "package identity unavailable")?;
                crate::fs::ensure_same_file(
                    &source.binding.root().join(&package_path),
                    &root_identity,
                    &current_identity,
                )
                .map_err(|_| "package replaced during discovery")?;
                let mut destination = paths.clone();
                let root = source
                    .binding
                    .root()
                    .to_str()
                    .ok_or("non-UTF8 source root")?;
                let namespace = Sha256::digest(
                    format!("{}\0{root}\0{}", source.origin, package_path.display()).as_bytes(),
                )
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
                let cache = paths
                    .cache_dir
                    .join("agent-skill-discovery")
                    .join(namespace);
                destination.data_dir = cache.join("data");
                destination.cache_dir = cache.join("cache");
                let installed = publish_tree(tree, Some(&name), &destination)
                    .map_err(|error| error.to_string())?;
                let mut catalogue = AgentSkillCatalog::new(&destination);
                let index = catalogue
                    .refresh_lexical()
                    .map_err(|error| error.to_string())?;
                let record = index
                    .records()
                    .iter()
                    .find(|record| record.digest == installed.identity.digest)
                    .ok_or("published skill missing from validated catalogue")?;
                result
                    .packages
                    .entry(record.name.clone())
                    .or_insert_with(|| Package {
                        record: Arc::clone(record),
                        origin: source.origin,
                        source: Some(source.clone()),
                        source_directory: package_path,
                        source_identity: Some(root_identity),
                        files,
                    });
                result.catalogues.push(catalogue);
                Ok(())
            })();
            if attempt.is_err() {
                result.diagnostics.push(format!(
                    "{}: package rejected by validation or source policy",
                    source.origin
                ));
                if entries > MAX_ENTRIES || bytes > MAX_SOURCE_BYTES {
                    return result;
                }
            }
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn collect(
    source: &SourceRoot,
    permission: &Option<PermCheck>,
    root: &Path,
    current: &Path,
    depth: usize,
    entries: &mut usize,
    bytes: &mut usize,
    tree: &mut SourceTree,
    files: &mut Vec<SourceFile>,
) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err("skill depth limit exceeded".into());
    }
    let directory = source
        .binding
        .open_dir_relative(current)
        .map_err(|_| "linked or unavailable directory")?;
    let held = crate::fs::checked_file_metadata(
        &directory
            .try_clone()
            .map_err(|_| "directory unavailable")?
            .into_std_file(),
    )
    .map_err(|_| "directory identity unavailable")?;
    for entry in directory
        .read_dir(".")
        .map_err(|_| "cannot enumerate package")?
    {
        *entries = entries.saturating_add(1);
        if *entries > MAX_ENTRIES {
            return Err("discovery entry limit exceeded".into());
        }
        let entry = entry.map_err(|_| "cannot enumerate package entry")?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "non-UTF8 resource name")?;
        let path = current.join(&name);
        let relative = path
            .strip_prefix(root)
            .map_err(|_| "resource escapes package")?
            .to_str()
            .ok_or("non-UTF8 resource path")?
            .replace('\\', "/");
        let kind = entry.file_type().map_err(|_| "cannot inspect resource")?;
        if kind.is_dir() {
            tree.insert_directory(&relative, true)
                .map_err(|error| error.to_string())?;
            collect(
                source,
                permission,
                root,
                &path,
                depth + 1,
                entries,
                bytes,
                tree,
                files,
            )?;
        } else {
            if !kind.is_file() || !allowed(&source.binding, &path, permission) {
                return Err(
                    "resource requires existing read permission and a real regular file".into(),
                );
            }
            let file = source
                .binding
                .open_regular_relative(&path)
                .map_err(|_| "resource is linked or not regular")?;
            let before = crate::fs::checked_file_metadata(&file)
                .map_err(|_| "resource identity unavailable")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if before.nlink() != 1 {
                    return Err("hard-linked resource refused".into());
                }
            }
            #[cfg(windows)]
            if crate::fs::windows_file_link_count(&file)
                .map_err(|_| "resource link count unavailable")?
                != 1
            {
                return Err("hard-linked resource refused".into());
            }
            let cap = if relative == "SKILL.md" {
                super::MAX_SKILL_INSTRUCTION_BYTES as usize
            } else {
                MAX_RESOURCE_BYTES
            };
            if before.len() > cap as u64
                || before.len() > MAX_SOURCE_BYTES.saturating_sub(*bytes) as u64
            {
                return Err("skill content byte limit exceeded".into());
            }
            let mut content = Vec::new();
            file.take(cap as u64 + 1)
                .read_to_end(&mut content)
                .map_err(|_| "cannot read resource")?;
            if content.len() > cap {
                return Err("resource grew beyond limit".into());
            }
            *bytes = bytes.saturating_add(content.len());
            if *bytes > MAX_SOURCE_BYTES {
                return Err("discovery content limit exceeded".into());
            }
            let after_file = source
                .binding
                .open_regular_relative(&path)
                .map_err(|_| "resource replaced")?;
            let after = crate::fs::checked_file_metadata(&after_file)
                .map_err(|_| "resource identity unavailable")?;
            crate::fs::ensure_same_file(&source.binding.root().join(&path), &before, &after)
                .map_err(|_| "resource replaced during read")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if after.nlink() != 1 {
                    return Err("resource link count changed".into());
                }
            }
            #[cfg(windows)]
            if crate::fs::windows_file_link_count(&after_file)
                .map_err(|_| "resource link count unavailable")?
                != 1
            {
                return Err("resource link count changed".into());
            }
            if before.len() != content.len() as u64
                || after.len() != before.len()
                || after.modified().ok() != before.modified().ok()
            {
                return Err("resource changed during read".into());
            }
            let stamp = Arc::new(OnceLock::new());
            if let Some(recorded) = quiet_file_stamp(&before) {
                let _ = stamp.set(recorded);
            }
            files.push(SourceFile {
                path,
                metadata: before,
                hash: Sha256::digest(&content).to_vec(),
                stamp,
                #[cfg(test)]
                full_reads: Arc::default(),
            });
            tree.insert_file(&relative, content)
                .map_err(|error| error.to_string())?;
        }
    }
    let after_file = source
        .binding
        .open_relative_directory_file(current)
        .map_err(|_| "directory replaced")?;
    let after = crate::fs::checked_file_metadata(&after_file)
        .map_err(|_| "directory identity unavailable")?;
    crate::fs::ensure_same_file(&source.binding.root().join(current), &held, &after)
        .map_err(|_| "directory replaced during scan".to_string())
}
