use std::collections::BTreeMap;
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::paths::AppPaths;
use crate::{fs as secure_fs, paths::portable};

use super::manifest::{AgentSkillManifest, ManifestError, parse_skill_markdown};

const TREE_IDENTITY_VERSION: &[u8] = b"mini-agent-agent-skill-tree-v1";
const MAX_ARCHIVE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_EXPANDED_BYTES: u64 = 128 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;
const MAX_DEPTH: usize = 16;
const MAX_COMPRESSION_RATIO: u64 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeIdentity {
    pub version: u32,
    pub digest: String,
    pub entries: usize,
    pub files: usize,
    pub expanded_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct ImportedSkill {
    pub manifest: AgentSkillManifest,
    pub identity: TreeIdentity,
    pub install_path: PathBuf,
    pub reimported: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("Agent Skill source must be one real directory or one .zip file")]
    InvalidSource,
    #[error("Agent Skill source path is not valid UTF-8")]
    NonUtf8Path,
    #[error("Agent Skill tree contains a link, reparse point, or non-regular entry: {0}")]
    UnsafeEntry(String),
    #[error("Agent Skill tree exceeds the {limit} limit")]
    LimitExceeded { limit: &'static str },
    #[error("Agent Skill archive contains an unsafe path: {0}")]
    UnsafePath(String),
    #[error("Agent Skill tree contains duplicate or portable-colliding path {0}")]
    PathCollision(String),
    #[error("Agent Skill tree must contain exactly one SKILL.md")]
    InvalidSkillRoot,
    #[error("Agent Skill archive must contain only one root skill tree")]
    MultipleRoots,
    #[error("frontmatter name {manifest:?} does not match directory name {directory:?}")]
    NameDirectoryMismatch { manifest: String, directory: String },
    #[error("installed Agent Skill tree failed content verification")]
    VerificationFailed,
    #[error("installed Agent Skill digest path already contains different content")]
    DigestConflict,
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    PortablePath(#[from] portable::PortablePathError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
}

#[derive(Clone, Debug)]
struct TreeEntry {
    bytes: Option<Vec<u8>>,
    explicit_directory: bool,
}

#[derive(Default)]
struct SourceTree {
    entries: BTreeMap<String, TreeEntry>,
    collision_paths: BTreeMap<String, String>,
}

impl SourceTree {
    fn insert_directory(&mut self, path: &str, explicit: bool) -> Result<(), ImportError> {
        self.insert(path, None, explicit)
    }

    fn insert_file(&mut self, path: &str, bytes: Vec<u8>) -> Result<(), ImportError> {
        self.insert(path, Some(bytes), false)
    }

    fn insert(
        &mut self,
        path: &str,
        bytes: Option<Vec<u8>>,
        explicit_directory: bool,
    ) -> Result<(), ImportError> {
        validate_tree_path(path)?;
        let components: Vec<&str> = path.split('/').collect();
        let mut exact = String::new();
        let mut collision = String::new();

        for (index, component) in components.iter().enumerate() {
            if index > 0 {
                exact.push('/');
                collision.push('/');
            }
            exact.push_str(component);
            collision.push_str(&portable::collision_key(component)?);

            let is_leaf = index + 1 == components.len();
            if let Some(previous) = self.collision_paths.get(&collision) {
                if previous != &exact {
                    return Err(ImportError::PathCollision(path.to_owned()));
                }
            } else {
                self.collision_paths
                    .insert(collision.clone(), exact.clone());
            }

            if !is_leaf {
                match self.entries.get(&exact) {
                    Some(entry) if entry.bytes.is_some() => {
                        return Err(ImportError::PathCollision(path.to_owned()));
                    }
                    Some(_) => {}
                    None => {
                        self.entries.insert(
                            exact.clone(),
                            TreeEntry {
                                bytes: None,
                                explicit_directory: false,
                            },
                        );
                    }
                }
                continue;
            }

            match (&bytes, self.entries.get_mut(&exact)) {
                (Some(_), Some(_)) => {
                    return Err(ImportError::PathCollision(path.to_owned()));
                }
                (Some(contents), None) => {
                    self.entries.insert(
                        exact.clone(),
                        TreeEntry {
                            bytes: Some(contents.clone()),
                            explicit_directory: false,
                        },
                    );
                }
                (None, Some(entry)) if entry.bytes.is_some() || entry.explicit_directory => {
                    return Err(ImportError::PathCollision(path.to_owned()));
                }
                (None, Some(entry)) => entry.explicit_directory = explicit_directory,
                (None, None) => {
                    self.entries.insert(
                        exact.clone(),
                        TreeEntry {
                            bytes: None,
                            explicit_directory,
                        },
                    );
                }
            }
        }

        if self.entries.len() > MAX_ENTRIES {
            return Err(ImportError::LimitExceeded {
                limit: "entry-count",
            });
        }
        Ok(())
    }

    fn file(&self, path: &str) -> Option<&[u8]> {
        self.entries.get(path)?.bytes.as_deref()
    }
}

/// Validate and durably install one local Agent Skills directory or ZIP.
///
/// This function only reads and copies resources. It never executes a file,
/// grants a tool permission, or inserts bundled JavaScript into a learned
/// skill store.
pub fn import_agent_skill(
    source: &Path,
    app_paths: &AppPaths,
) -> Result<ImportedSkill, ImportError> {
    let source_metadata = fs::symlink_metadata(source).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ImportError::InvalidSource
        } else {
            ImportError::Io(error)
        }
    })?;
    if portable::is_link_or_reparse(&source_metadata) {
        return Err(ImportError::UnsafeEntry(source.display().to_string()));
    }

    let (tree, expected_directory) = if source_metadata.is_dir() {
        let directory_name = source
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(ImportError::NonUtf8Path)?
            .to_owned();
        portable::validate_portable_component(&directory_name)?;
        (collect_directory(source)?, Some(directory_name))
    } else if source_metadata.is_file()
        && source
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
    {
        (collect_zip(source)?, None)
    } else {
        return Err(ImportError::InvalidSource);
    };

    let (tree, manifest) = normalize_skill_tree(tree, expected_directory.as_deref())?;
    let identity = identity(&tree);

    let staging_parent = app_paths.cache_dir.join("import-staging");
    secure_fs::ensure_private_directory(&staging_parent)?;
    let staging_path = staging_parent.join(format!("agent-skill-{}", uuid::Uuid::new_v4()));
    let mut staging_cleanup = CleanupDirectory::new(staging_path.clone());
    materialize(&staging_path, &tree)?;
    verify_materialized(&staging_path, &identity.digest)?;

    let install_root = app_paths.data_dir.join("agent-skills");
    secure_fs::ensure_private_directory(&install_root)?;
    let name_root = install_root.join(&manifest.name);
    portable::ensure_no_link_traversal(&install_root, &name_root)?;
    secure_fs::ensure_private_directory(&name_root)?;
    let install_path = name_root.join(&identity.digest);
    portable::ensure_no_link_traversal(&install_root, &install_path)?;

    if fs::symlink_metadata(&install_path).is_ok() {
        validate_existing(&install_path, &manifest.name, &identity.digest)?;
        staging_cleanup.remove_now()?;
        return Ok(ImportedSkill {
            manifest,
            identity,
            install_path,
            reimported: true,
        });
    }

    let publication_path = name_root.join(format!(".import-{}", uuid::Uuid::new_v4()));
    let mut publication_cleanup = CleanupDirectory::new(publication_path.clone());
    materialize(&publication_path, &tree)?;
    verify_materialized(&publication_path, &identity.digest)?;
    make_tree_read_only(&publication_path)?;

    match fs::rename(&publication_path, &install_path) {
        Ok(()) => publication_cleanup.disarm(),
        Err(_rename_error) if fs::symlink_metadata(&install_path).is_ok() => {
            validate_existing(&install_path, &manifest.name, &identity.digest)?;
            publication_cleanup.remove_now()?;
            staging_cleanup.remove_now()?;
            return Ok(ImportedSkill {
                manifest,
                identity,
                install_path,
                reimported: true,
            });
        }
        Err(rename_error) => return Err(ImportError::Io(rename_error)),
    }

    staging_cleanup.remove_now()?;
    Ok(ImportedSkill {
        manifest,
        identity,
        install_path,
        reimported: false,
    })
}

fn collect_directory(root: &Path) -> Result<SourceTree, ImportError> {
    let mut tree = SourceTree::default();
    let mut raw_entries = 0usize;
    let mut expanded_bytes = 0u64;
    collect_directory_recursive(root, root, &mut tree, &mut raw_entries, &mut expanded_bytes)?;
    Ok(tree)
}

fn collect_directory_recursive(
    root: &Path,
    current: &Path,
    tree: &mut SourceTree,
    raw_entries: &mut usize,
    expanded_bytes: &mut u64,
) -> Result<(), ImportError> {
    let before = fs::symlink_metadata(current)?;
    if portable::is_link_or_reparse(&before) || !before.is_dir() {
        return Err(ImportError::UnsafeEntry(current.display().to_string()));
    }

    let mut entries = fs::read_dir(current)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        *raw_entries += 1;
        if *raw_entries > MAX_ENTRIES {
            return Err(ImportError::LimitExceeded {
                limit: "entry-count",
            });
        }

        let path = entry.path();
        let relative = portable_relative(root, &path)?;
        let metadata = fs::symlink_metadata(&path)?;
        if portable::is_link_or_reparse(&metadata) {
            return Err(ImportError::UnsafeEntry(relative));
        }
        if metadata.is_dir() {
            tree.insert_directory(&relative, true)?;
            collect_directory_recursive(root, &path, tree, raw_entries, expanded_bytes)?;
        } else if metadata.is_file() {
            if metadata.len() > MAX_FILE_BYTES {
                return Err(ImportError::LimitExceeded {
                    limit: "per-file-bytes",
                });
            }
            let bytes = read_stable_file(&path, MAX_FILE_BYTES)?;
            *expanded_bytes = expanded_bytes.checked_add(bytes.len() as u64).ok_or(
                ImportError::LimitExceeded {
                    limit: "expanded-bytes",
                },
            )?;
            if *expanded_bytes > MAX_EXPANDED_BYTES {
                return Err(ImportError::LimitExceeded {
                    limit: "expanded-bytes",
                });
            }
            tree.insert_file(&relative, bytes)?;
        } else {
            return Err(ImportError::UnsafeEntry(relative));
        }
    }

    let after = fs::symlink_metadata(current)?;
    secure_fs::ensure_same_file(current, &before, &after)?;
    Ok(())
}

fn collect_zip(source: &Path) -> Result<SourceTree, ImportError> {
    let archive_bytes = read_stable_file(source, MAX_ARCHIVE_BYTES)?;
    let mut archive = zip::ZipArchive::new(Cursor::new(archive_bytes))?;
    if archive.len() > MAX_ENTRIES {
        return Err(ImportError::LimitExceeded {
            limit: "entry-count",
        });
    }
    if archive.has_overlapping_files()? {
        return Err(ImportError::UnsafeEntry(
            "overlapping ZIP data ranges".to_owned(),
        ));
    }

    let mut tree = SourceTree::default();
    let mut compressed_bytes = 0u64;
    let mut expanded_bytes = 0u64;

    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        if file.encrypted() {
            return Err(ImportError::UnsafeEntry(
                "encrypted ZIP entries are unsupported".to_owned(),
            ));
        }

        let raw_name = std::str::from_utf8(file.name_raw())
            .map_err(|_| ImportError::NonUtf8Path)?
            .to_owned();
        let is_directory = file.is_dir();
        let normalized_name = if is_directory {
            raw_name.strip_suffix('/').unwrap_or(&raw_name)
        } else {
            raw_name.as_str()
        };
        if normalized_name.is_empty() {
            return Err(ImportError::UnsafePath(raw_name));
        }
        validate_zip_entry_type(&file, normalized_name)?;
        validate_tree_path(normalized_name)?;

        compressed_bytes = compressed_bytes.checked_add(file.compressed_size()).ok_or(
            ImportError::LimitExceeded {
                limit: "compressed-bytes",
            },
        )?;
        expanded_bytes =
            expanded_bytes
                .checked_add(file.size())
                .ok_or(ImportError::LimitExceeded {
                    limit: "expanded-bytes",
                })?;
        if compressed_bytes > MAX_ARCHIVE_BYTES {
            return Err(ImportError::LimitExceeded {
                limit: "compressed-bytes",
            });
        }
        if expanded_bytes > MAX_EXPANDED_BYTES {
            return Err(ImportError::LimitExceeded {
                limit: "expanded-bytes",
            });
        }
        validate_ratio(file.size(), file.compressed_size())?;

        if is_directory {
            tree.insert_directory(normalized_name, true)?;
            continue;
        }
        if file.size() > MAX_FILE_BYTES {
            return Err(ImportError::LimitExceeded {
                limit: "per-file-bytes",
            });
        }

        let mut bytes = Vec::with_capacity(file.size() as usize);
        file.by_ref()
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != file.size() {
            return Err(ImportError::VerificationFailed);
        }
        tree.insert_file(normalized_name, bytes)?;
    }
    validate_ratio(expanded_bytes, compressed_bytes)?;
    Ok(tree)
}

fn validate_zip_entry_type<R: Read>(
    file: &zip::read::ZipFile<'_, R>,
    name: &str,
) -> Result<(), ImportError> {
    if file.is_symlink() || (!file.is_dir() && !file.is_file()) {
        return Err(ImportError::UnsafeEntry(name.to_owned()));
    }
    if let Some(mode) = file.unix_mode() {
        let file_type = mode & 0o170000;
        let expected = if file.is_dir() { 0o040000 } else { 0o100000 };
        if file_type != 0 && file_type != expected {
            return Err(ImportError::UnsafeEntry(name.to_owned()));
        }
    }
    Ok(())
}

fn validate_ratio(expanded: u64, compressed: u64) -> Result<(), ImportError> {
    if expanded == 0 {
        return Ok(());
    }
    if compressed == 0 || expanded > compressed.saturating_mul(MAX_COMPRESSION_RATIO) {
        return Err(ImportError::LimitExceeded {
            limit: "compression-ratio",
        });
    }
    Ok(())
}

fn normalize_skill_tree(
    tree: SourceTree,
    expected_directory: Option<&str>,
) -> Result<(SourceTree, AgentSkillManifest), ImportError> {
    let skill_paths: Vec<&str> = tree
        .entries
        .iter()
        .filter_map(|(path, entry)| {
            (entry.bytes.is_some() && path.split('/').next_back() == Some("SKILL.md"))
                .then_some(path.as_str())
        })
        .collect();
    if skill_paths.len() != 1 {
        return Err(ImportError::InvalidSkillRoot);
    }

    let skill_path = skill_paths[0];
    let (normalized, directory_name) = match expected_directory {
        Some(directory_name) => {
            if skill_path != "SKILL.md" {
                return Err(ImportError::InvalidSkillRoot);
            }
            (tree, directory_name.to_owned())
        }
        None if skill_path == "SKILL.md" => {
            let manifest =
                parse_skill_markdown(tree.file("SKILL.md").ok_or(ImportError::InvalidSkillRoot)?)?;
            let directory_name = manifest.name.clone();
            return Ok((tree, manifest_for_directory(manifest, &directory_name)?));
        }
        None => {
            let components: Vec<&str> = skill_path.split('/').collect();
            if components.len() != 2 || components[1] != "SKILL.md" {
                return Err(ImportError::InvalidSkillRoot);
            }
            let directory_name = components[0].to_owned();
            let normalized = strip_single_root(tree, &directory_name)?;
            (normalized, directory_name)
        }
    };

    let manifest = parse_skill_markdown(
        normalized
            .file("SKILL.md")
            .ok_or(ImportError::InvalidSkillRoot)?,
    )?;
    Ok((
        normalized,
        manifest_for_directory(manifest, &directory_name)?,
    ))
}

fn manifest_for_directory(
    manifest: AgentSkillManifest,
    directory_name: &str,
) -> Result<AgentSkillManifest, ImportError> {
    if manifest.name != directory_name {
        return Err(ImportError::NameDirectoryMismatch {
            manifest: manifest.name,
            directory: directory_name.to_owned(),
        });
    }
    Ok(manifest)
}

fn strip_single_root(tree: SourceTree, root: &str) -> Result<SourceTree, ImportError> {
    let mut normalized = SourceTree::default();
    let prefix = format!("{root}/");
    for (path, entry) in tree.entries {
        if path == root && entry.bytes.is_none() {
            continue;
        }
        let relative = path
            .strip_prefix(&prefix)
            .ok_or(ImportError::MultipleRoots)?;
        match entry.bytes {
            Some(bytes) => normalized.insert_file(relative, bytes)?,
            None => normalized.insert_directory(relative, entry.explicit_directory)?,
        }
    }
    Ok(normalized)
}

fn identity(tree: &SourceTree) -> TreeIdentity {
    let mut hasher = Sha256::new();
    update_digest(&mut hasher, TREE_IDENTITY_VERSION);
    hasher.update((tree.entries.len() as u64).to_be_bytes());
    let mut files = 0usize;
    let mut expanded_bytes = 0u64;
    for (path, entry) in &tree.entries {
        update_digest(&mut hasher, path.as_bytes());
        match &entry.bytes {
            Some(bytes) => {
                hasher.update([1]);
                update_digest(&mut hasher, bytes);
                files += 1;
                expanded_bytes += bytes.len() as u64;
            }
            None => hasher.update([0]),
        }
    }
    TreeIdentity {
        version: 1,
        digest: hex_digest(hasher.finalize()),
        entries: tree.entries.len(),
        files,
        expanded_bytes,
    }
}

fn materialize(root: &Path, tree: &SourceTree) -> Result<(), ImportError> {
    secure_fs::ensure_private_directory(root)?;
    for (relative, entry) in &tree.entries {
        let path = portable::contained_join(root, Path::new(relative))?;
        portable::ensure_no_link_traversal(root, &path)?;
        match &entry.bytes {
            None => secure_fs::ensure_private_directory(&path)?,
            Some(bytes) => {
                let parent = path.parent().ok_or(ImportError::VerificationFailed)?;
                secure_fs::ensure_private_directory(parent)?;
                secure_fs::private_atomic_create_sync(&path, bytes)?;
            }
        }
    }
    Ok(())
}

fn verify_materialized(root: &Path, digest: &str) -> Result<(), ImportError> {
    let observed = collect_directory(root)?;
    if identity(&observed).digest != digest {
        return Err(ImportError::VerificationFailed);
    }
    Ok(())
}

fn validate_existing(root: &Path, name: &str, digest: &str) -> Result<(), ImportError> {
    let metadata = fs::symlink_metadata(root)?;
    if portable::is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(ImportError::DigestConflict);
    }
    let tree = collect_directory(root)?;
    let manifest = parse_skill_markdown(tree.file("SKILL.md").ok_or(ImportError::DigestConflict)?)
        .map_err(|_| ImportError::DigestConflict)?;
    if manifest.name != name || identity(&tree).digest != digest {
        return Err(ImportError::DigestConflict);
    }
    Ok(())
}

fn validate_tree_path(path: &str) -> Result<(), ImportError> {
    if path.contains('\\') {
        return Err(ImportError::UnsafePath(path.to_owned()));
    }
    portable::validate_portable_relative_path(Path::new(path))
        .map_err(|_| ImportError::UnsafePath(path.to_owned()))?;
    if path.split('/').count() > MAX_DEPTH {
        return Err(ImportError::LimitExceeded {
            limit: "path-depth",
        });
    }
    Ok(())
}

fn portable_relative(root: &Path, path: &Path) -> Result<String, ImportError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| ImportError::UnsafePath(path.display().to_string()))?;
    let mut components = Vec::new();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(ImportError::UnsafePath(path.display().to_string()));
        };
        components.push(
            component
                .to_str()
                .ok_or(ImportError::NonUtf8Path)?
                .to_owned(),
        );
    }
    let relative = components.join("/");
    validate_tree_path(&relative)?;
    Ok(relative)
}

fn read_stable_file(path: &Path, limit: u64) -> Result<Vec<u8>, ImportError> {
    let before = fs::symlink_metadata(path)?;
    if portable::is_link_or_reparse(&before) || !before.is_file() {
        return Err(ImportError::UnsafeEntry(path.display().to_string()));
    }
    if before.len() > limit {
        return Err(ImportError::LimitExceeded {
            limit: if limit == MAX_ARCHIVE_BYTES {
                "compressed-bytes"
            } else {
                "per-file-bytes"
            },
        });
    }

    let file = open_source_file(path)?;
    let opened = file.metadata()?;
    secure_fs::ensure_same_file(path, &before, &opened)?;
    let mut bytes = Vec::with_capacity(before.len() as usize);
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit || bytes.len() as u64 != before.len() {
        return Err(ImportError::VerificationFailed);
    }
    let after = fs::symlink_metadata(path)?;
    secure_fs::ensure_same_file(path, &opened, &after)?;
    Ok(bytes)
}

#[cfg(unix)]
fn open_source_file(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    #[cfg(target_os = "linux")]
    const NO_FOLLOW: i32 = 0x2_0000;
    #[cfg(target_os = "linux")]
    const NON_BLOCK: i32 = 0x800;
    #[cfg(target_os = "linux")]
    const CLOSE_ON_EXEC: i32 = 0x8_0000;
    #[cfg(target_os = "macos")]
    const NO_FOLLOW: i32 = 0x100;
    #[cfg(target_os = "macos")]
    const NON_BLOCK: i32 = 0x4;
    #[cfg(target_os = "macos")]
    const CLOSE_ON_EXEC: i32 = 0x100_0000;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    const NO_FOLLOW: i32 = 0;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    const NON_BLOCK: i32 = 0;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    const CLOSE_ON_EXEC: i32 = 0;

    fs::OpenOptions::new()
        .read(true)
        .custom_flags(NO_FOLLOW | NON_BLOCK | CLOSE_ON_EXEC)
        .open(path)
}

#[cfg(windows)]
fn open_source_file(path: &Path) -> std::io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;

    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_source_file(path: &Path) -> std::io::Result<fs::File> {
    fs::File::open(path)
}

fn make_tree_read_only(root: &Path) -> Result<(), ImportError> {
    let tree = collect_directory(root)?;
    for (relative, entry) in &tree.entries {
        if entry.bytes.is_some() {
            set_read_only(&portable::contained_join(root, Path::new(relative))?, false)?;
        }
    }
    let mut directories: Vec<&str> = tree
        .entries
        .iter()
        .filter_map(|(path, entry)| entry.bytes.is_none().then_some(path.as_str()))
        .collect();
    directories.sort_by_key(|path| std::cmp::Reverse(path.split('/').count()));
    for relative in directories {
        set_read_only(&portable::contained_join(root, Path::new(relative))?, true)?;
    }
    set_read_only(root, true)?;
    Ok(())
}

#[cfg(unix)]
fn set_read_only(path: &Path, directory: bool) -> Result<(), ImportError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if directory { 0o500 } else { 0o400 }),
    )?;
    Ok(())
}

#[cfg(windows)]
fn set_read_only(path: &Path, directory: bool) -> Result<(), ImportError> {
    if !directory {
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn set_read_only(_path: &Path, _directory: bool) -> Result<(), ImportError> {
    Err(ImportError::Io(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Agent Skill installation is unsupported on this platform",
    )))
}

fn update_digest(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

struct CleanupDirectory {
    path: Option<PathBuf>,
}

impl CleanupDirectory {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }

    fn remove_now(&mut self) -> Result<(), ImportError> {
        if let Some(path) = self.path.take() {
            remove_tree_no_follow(&path)?;
        }
        Ok(())
    }
}

impl Drop for CleanupDirectory {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = remove_tree_no_follow(&path);
        }
    }
}

fn remove_tree_no_follow(path: &Path) -> std::io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if portable::is_link_or_reparse(&metadata) || !metadata.is_dir() {
        make_writable(path, false)?;
        return fs::remove_file(path);
    }

    make_writable(path, true)?;
    for entry in fs::read_dir(path)? {
        remove_tree_no_follow(&entry?.path())?;
    }
    fs::remove_dir(path)
}

#[cfg(unix)]
fn make_writable(path: &Path, directory: bool) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if directory { 0o700 } else { 0o600 }),
    )
}

#[cfg(windows)]
fn make_writable(path: &Path, _directory: bool) -> std::io::Result<()> {
    let mut permissions = fs::symlink_metadata(path)?.permissions();
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions)
}

#[cfg(not(any(unix, windows)))]
fn make_writable(_path: &Path, _directory: bool) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("mini-agent-skill-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn paths(&self) -> AppPaths {
            AppPaths {
                config_dir: self.0.join("config"),
                data_dir: self.0.join("data"),
                local_data_dir: self.0.join("local-data"),
                state_dir: self.0.join("state"),
                cache_dir: self.0.join("cache"),
                credentials_dir: self.0.join("credentials"),
                project_dir: None,
            }
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = remove_tree_no_follow(&self.0);
        }
    }

    fn skill_markdown(name: &str) -> Vec<u8> {
        format!(
            "---\nname: {name}\ndescription: Imports {name} when requested.\nallowed-tools: Bash(echo:*)\n---\n\n# Instructions\n"
        )
        .into_bytes()
    }

    fn write_directory_skill(root: &Path, name: &str, marker: &Path) -> PathBuf {
        let skill = root.join(name);
        fs::create_dir_all(skill.join("assets")).unwrap();
        fs::create_dir_all(skill.join("scripts")).unwrap();
        fs::write(skill.join("SKILL.md"), skill_markdown(name)).unwrap();
        fs::write(skill.join("assets").join("note.txt"), b"exact bytes\n").unwrap();
        fs::write(
            skill.join("scripts").join("never-run.sh"),
            format!("#!/bin/sh\ntouch {}\n", marker.display()),
        )
        .unwrap();
        skill
    }

    fn write_root_zip(path: &Path, name: &str, marker: &Path) {
        let file = fs::File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        writer.start_file("SKILL.md", options).unwrap();
        writer.write_all(&skill_markdown(name)).unwrap();
        writer.add_directory("assets/", options).unwrap();
        writer.start_file("assets/note.txt", options).unwrap();
        writer.write_all(b"exact bytes\n").unwrap();
        writer.add_directory("scripts/", options).unwrap();
        writer.start_file("scripts/never-run.sh", options).unwrap();
        writer
            .write_all(format!("#!/bin/sh\ntouch {}\n", marker.display()).as_bytes())
            .unwrap();
        writer.finish().unwrap();
    }

    #[test]
    fn agent_skill_import_checked_in_evidence_fixture_installs() {
        let temp = TempRoot::new();
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src/extras/skills/fixtures/evidence-skill");

        let imported = import_agent_skill(&fixture, &temp.paths()).unwrap();

        assert_eq!(imported.manifest.name, "evidence-skill");
        assert_eq!(
            imported.manifest.allowed_tools.as_deref(),
            Some("Bash(example:*)")
        );
        assert_eq!(
            fs::read(fixture.join("SKILL.md")).unwrap(),
            fs::read(imported.install_path.join("SKILL.md")).unwrap()
        );
        assert!(imported.install_path.join("SKILL.md").is_file());
        assert!(
            imported
                .install_path
                .join("scripts")
                .join("never-run.sh")
                .is_file()
        );
    }

    #[test]
    fn agent_skill_import_directory_and_root_zip_have_identical_identity() {
        let temp = TempRoot::new();
        let marker = temp.0.join("executed");
        let directory = write_directory_skill(&temp.0, "same-skill", &marker);
        let zip_path = temp.0.join("arbitrary-name.zip");
        write_root_zip(&zip_path, "same-skill", &marker);

        let from_directory = import_agent_skill(&directory, &temp.paths()).unwrap();
        let from_zip = import_agent_skill(&zip_path, &temp.paths()).unwrap();

        assert_eq!(from_directory.identity.digest, from_zip.identity.digest);
        assert!(from_zip.reimported);
        assert!(!marker.exists());
        assert_eq!(
            from_zip.manifest.allowed_tools.as_deref(),
            Some("Bash(echo:*)")
        );
        assert_eq!(
            from_zip.install_path,
            temp.paths()
                .data_dir
                .join("agent-skills")
                .join("same-skill")
                .join(&from_zip.identity.digest)
        );
    }

    #[test]
    fn agent_skill_import_top_level_directory_zip_installs() {
        let temp = TempRoot::new();
        let zip_path = temp.0.join("skill.zip");
        let file = fs::File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        writer.add_directory("nested-skill/", options).unwrap();
        writer.start_file("nested-skill/SKILL.md", options).unwrap();
        writer.write_all(&skill_markdown("nested-skill")).unwrap();
        writer.finish().unwrap();

        let imported = import_agent_skill(&zip_path, &temp.paths()).unwrap();
        assert_eq!(imported.manifest.name, "nested-skill");
        assert!(imported.install_path.join("SKILL.md").is_file());
    }

    #[test]
    fn agent_skill_import_adversarial_rejects_traversal_and_cleans_staging() {
        let temp = TempRoot::new();
        let zip_path = temp.0.join("traversal.zip");
        let file = fs::File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        writer.start_file("../escape", options).unwrap();
        writer.write_all(b"escape").unwrap();
        writer.finish().unwrap();

        assert!(import_agent_skill(&zip_path, &temp.paths()).is_err());
        let staging = temp.paths().cache_dir.join("import-staging");
        assert!(!staging.exists() || fs::read_dir(staging).unwrap().next().is_none());
        assert!(!temp.0.join("escape").exists());
    }

    #[test]
    fn agent_skill_import_adversarial_rejects_case_folded_collisions() {
        let temp = TempRoot::new();
        let zip_path = temp.0.join("collision.zip");
        let file = fs::File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        writer.start_file("collision/SKILL.md", options).unwrap();
        writer.write_all(&skill_markdown("collision")).unwrap();
        writer.start_file("collision/Readme", options).unwrap();
        writer.write_all(b"one").unwrap();
        writer.start_file("collision/README", options).unwrap();
        writer.write_all(b"two").unwrap();
        writer.finish().unwrap();

        assert!(matches!(
            import_agent_skill(&zip_path, &temp.paths()),
            Err(ImportError::PathCollision(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn agent_skill_import_adversarial_rejects_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = TempRoot::new();
        let marker = temp.0.join("executed");
        let directory = write_directory_skill(&temp.0, "linked-skill", &marker);
        symlink(
            temp.0.join("outside"),
            directory.join("assets").join("link"),
        )
        .unwrap();
        assert!(matches!(
            import_agent_skill(&directory, &temp.paths()),
            Err(ImportError::UnsafeEntry(_))
        ));
    }
}
