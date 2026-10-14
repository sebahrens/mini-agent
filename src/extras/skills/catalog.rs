//! Immutable metadata catalog for installed instruction-only Agent Skills.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

#[cfg(feature = "skills")]
use crate::extras::js::skills::embed::Embedder;
use crate::paths::{AppPaths, portable};

use super::index::{AgentSkillIndex, AgentSkillRecord};
use super::manifest::parse_skill_markdown;
use super::retention::SessionLease;

// A tree whose SKILL.md is larger than one turn's instruction budget can never
// be surfaced, so it is omitted from the generation rather than ranked first
// and then dropped. Import refuses to install one above this bound.
const MAX_SKILL_MD_BYTES: u64 = super::MAX_SKILL_INSTRUCTION_BYTES;
const MAX_ACTIVE_POINTER_BYTES: u64 = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceMetadata {
    pub relative_path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error(transparent)]
    StableRead(#[from] super::ImportError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Manifest(#[from] super::ManifestError),
    #[error(transparent)]
    Portable(#[from] portable::PortablePathError),
    #[cfg(feature = "skills")]
    #[error(transparent)]
    Embedding(#[from] crate::extras::js::skills::embed::EmbeddingError),
    #[error(transparent)]
    Index(#[from] super::index::AgentSkillIndexError),
    #[error("installed Agent Skill catalog contains an invalid digest path")]
    InvalidDigest,
    #[error("Agent Skill catalog exceeds its tree depth, entry count, or content byte limit")]
    ResourceLimit,
    #[error("active Agent Skill digest does not exist: {0}")]
    MissingActiveDigest(String),
    #[error(
        "installed Agent Skill {0} has {1} digests and no ACTIVE pointer; \
         re-import it to record which one is active"
    )]
    AmbiguousActiveDigest(String, usize),
}

/// Rebuildable catalog owner. Search readers receive only immutable `AgentSkillIndex` values.
pub struct AgentSkillCatalog {
    root: PathBuf,
    generation: u64,
    signature: Option<CatalogSignature>,
    lease_root: PathBuf,
    // Protects the digests this catalog's generations read from pruning while
    // the owning session runs; acquired with the first non-empty generation.
    lease: Option<SessionLease>,
}

// A signature is only ever compared as a whole. The derived `PartialEq` reads
// every field, but rustc does not count a derived impl as a read, so the allow
// is scoped to these two types instead of the whole `extras::skills` module.
#[derive(Clone, Eq, PartialEq)]
#[allow(dead_code)]
struct CatalogSignature {
    entries: Vec<CatalogSignatureEntry>,
    unavailable_packages: Vec<PathBuf>,
}

#[derive(Clone, Eq, PartialEq)]
#[allow(dead_code)]
struct CatalogSignatureEntry {
    path: PathBuf,
    bytes: u64,
    modified: Option<SystemTime>,
    kind: u8,
    pointer_digest: Option<String>,
}

impl AgentSkillCatalog {
    pub fn new(paths: &AppPaths) -> Self {
        Self {
            root: paths.data_dir.join("agent-skills"),
            generation: 0,
            signature: None,
            lease_root: super::retention::lease_root(&paths.data_dir),
            lease: None,
        }
    }

    /// Persist the active digest pointer with an atomic rename. This grants no authority.
    pub fn activate(&self, name: &str, digest: &str) -> Result<(), CatalogError> {
        validate_digest(digest)?;
        let name_root = self.root.join(name);
        let digest_root = name_root.join(digest);
        portable::ensure_no_link_traversal(&self.root, &digest_root)?;
        if !digest_root.join("SKILL.md").is_file() {
            return Err(CatalogError::MissingActiveDigest(digest.to_string()));
        }
        fs::create_dir_all(&name_root)?;
        crate::fs::private_atomic_write_sync(
            &name_root.join("ACTIVE"),
            format!("{digest}\n").as_bytes(),
        )?;
        Ok(())
    }

    /// Record the implicit active digest of a legacy package (exactly one
    /// digest and no ACTIVE pointer). Import calls this before publishing a
    /// second digest, so a process killed between that publication and the new
    /// pointer leaves the old version active instead of an ambiguous package
    /// the catalog omits.
    pub(crate) fn pin_legacy_active(&self, name: &str) -> Result<(), CatalogError> {
        let name_root = self.root.join(name);
        match fs::symlink_metadata(name_root.join("ACTIVE")) {
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        match select_active_digest(&name_root) {
            Ok(Some(digest)) => self.activate(name, &digest),
            // Nothing installed yet, or already ambiguous: the import's own
            // activation is what resolves it.
            Ok(None) | Err(CatalogError::AmbiguousActiveDigest(..)) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Scan installed immutable trees, batch metadata embeddings, and construct one generation.
    #[cfg(feature = "skills")]
    pub fn refresh(&mut self, embedder: &Embedder) -> Result<AgentSkillIndex, CatalogError> {
        // Capture the tree before scanning. An import racing this build changes
        // the next turn's signature and therefore forces another refresh.
        let signature = catalog_signature(&self.root)?;
        let mut records = self.scan_records(&signature, u64::MAX)?;
        self.record_lease(&records);
        let documents = records
            .iter()
            .map(AgentSkillRecord::embedding_document)
            .collect::<Vec<_>>();
        let vectors = if documents.is_empty() {
            Vec::new()
        } else {
            embedder.embed_documents(&documents)?
        };
        for (record, vector) in records.iter_mut().zip(vectors) {
            record.embedding = vector;
        }
        self.generation = self.generation.saturating_add(1);
        let index =
            AgentSkillIndex::build(self.generation, embedder.model_metadata().clone(), records)?;
        self.signature = Some(signature);
        Ok(index)
    }

    /// Instruction-only refresh: stable lexical ranking without inference,
    /// learned-store access, or a JavaScript worker.
    pub(crate) fn refresh_lexical(&mut self) -> Result<AgentSkillIndex, CatalogError> {
        let signature = catalog_signature(&self.root)?;
        let records = self.scan_records(&signature, 8 * 1024 * 1024)?;
        self.record_lease(&records);
        self.generation = self.generation.saturating_add(1);
        let index = AgentSkillIndex::build(
            self.generation,
            super::retrieval::ModelMetadata {
                model_id: "lexical-only".into(),
                model_revision: "agent-skills-lexical-v1".into(),
                dimensions: 0,
                normalized: false,
            },
            records,
        )?;
        self.signature = Some(signature);
        Ok(index)
    }

    fn scan_records(
        &self,
        signature: &CatalogSignature,
        mut bytes_left: u64,
    ) -> Result<Vec<AgentSkillRecord>, CatalogError> {
        let mut records = Vec::new();
        if self.root.is_dir() {
            let names = bounded_directory_entries(&self.root)?;
            for name_entry in names {
                if signature.unavailable_packages.contains(&name_entry.path())
                    || !name_entry.file_type().is_ok_and(|kind| kind.is_dir())
                {
                    continue;
                }
                if bytes_left == 0 {
                    break;
                }
                if let Ok(Some(record)) = scan_record(&self.root, &name_entry, &mut bytes_left) {
                    records.push(record);
                }
            }
        }
        Ok(records)
    }

    /// Lease the digests of a generation before any reader can load from it.
    /// Best effort: without a lease the retention window alone protects them.
    fn record_lease(&mut self, records: &[AgentSkillRecord]) {
        if self.lease.is_none() {
            if records.is_empty() {
                return;
            }
            match SessionLease::acquire(&self.lease_root) {
                Ok(lease) => self.lease = Some(lease),
                Err(error) => {
                    tracing::debug!("could not create Agent Skill session lease: {error}");
                    return;
                }
            }
        }
        if let Some(lease) = self.lease.as_mut()
            && let Err(error) = lease.record(
                records
                    .iter()
                    .map(|record| (record.name.as_str(), record.digest.as_str())),
            )
        {
            tracing::debug!("could not record Agent Skill session lease: {error}");
        }
    }

    /// Rebuild only after an import or ACTIVE-pointer change is visible.
    #[cfg(feature = "skills")]
    pub fn refresh_if_changed(
        &mut self,
        embedder: &Embedder,
    ) -> Result<Option<AgentSkillIndex>, CatalogError> {
        let current = catalog_signature(&self.root)?;
        if self.signature.as_ref() == Some(&current) {
            return Ok(None);
        }
        self.refresh(embedder).map(Some)
    }
}

fn signature_entry(path: PathBuf) -> Result<CatalogSignatureEntry, CatalogError> {
    let metadata = fs::symlink_metadata(&path)?;
    let kind = if metadata.file_type().is_symlink() {
        2
    } else if metadata.is_dir() {
        1
    } else {
        0
    };
    let pointer_digest = (path.file_name().is_some_and(|name| name == "ACTIVE")
        && metadata.is_file())
    .then(|| {
        super::import::read_stable_file(&path, MAX_ACTIVE_POINTER_BYTES, false)
            .map(|bytes| sha256_hex(&bytes))
    })
    .transpose()?;
    Ok(CatalogSignatureEntry {
        path,
        bytes: metadata.len(),
        modified: metadata.modified().ok(),
        kind,
        pointer_digest,
    })
}

fn bounded_directory_entries(path: &Path) -> Result<Vec<fs::DirEntry>, CatalogError> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path)? {
        if entries.len() == 1024 {
            return Err(CatalogError::ResourceLimit);
        }
        entries.push(entry?);
    }
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

fn catalog_signature(root: &Path) -> Result<CatalogSignature, CatalogError> {
    let root_entry = match signature_entry(root.to_path_buf()) {
        Ok(entry) => entry,
        Err(CatalogError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CatalogSignature {
                entries: Vec::new(),
                unavailable_packages: Vec::new(),
            });
        }
        Err(error) => return Err(error),
    };
    let mut entries = vec![root_entry];
    let mut unavailable_packages = Vec::new();
    let names = bounded_directory_entries(root)?;
    for name in names {
        let package_entries = (|| -> Result<Vec<CatalogSignatureEntry>, CatalogError> {
            let mut entries = vec![signature_entry(name.path())?];
            if name.file_type()?.is_dir() {
                let children = bounded_directory_entries(&name.path())?;
                for child in children {
                    entries.push(signature_entry(child.path())?);
                }
            }
            Ok(entries)
        })();
        match package_entries {
            Ok(package_entries) => {
                if entries.len().saturating_add(package_entries.len()) > 4096 {
                    return Err(CatalogError::ResourceLimit);
                }
                entries.extend(package_entries);
            }
            // Retry the package signature on every refresh check. Recording its unavailable
            // state prevents unnecessary index rebuilds while still detecting repairs.
            Err(_) => unavailable_packages.push(name.path()),
        }
    }
    Ok(CatalogSignature {
        entries,
        unavailable_packages,
    })
}

fn scan_record(
    root: &Path,
    name_entry: &fs::DirEntry,
    bytes_left: &mut u64,
) -> Result<Option<AgentSkillRecord>, CatalogError> {
    let name_root = name_entry.path();
    portable::ensure_no_link_traversal(root, &name_root)?;
    let Some(digest) = select_active_digest(&name_root)? else {
        return Ok(None);
    };
    let digest_root = name_root.join(&digest);
    portable::ensure_no_link_traversal(root, &digest_root)?;
    let markdown_path = digest_root.join("SKILL.md");
    let metadata = fs::symlink_metadata(&markdown_path)?;
    if !metadata.is_file() || metadata.len() > MAX_SKILL_MD_BYTES || metadata.len() > *bytes_left {
        return Ok(None);
    }
    *bytes_left = bytes_left.saturating_sub(metadata.len());
    let markdown = super::import::read_stable_file(&markdown_path, MAX_SKILL_MD_BYTES, false)?;
    let manifest = parse_skill_markdown(&markdown)?;
    if name_entry.file_name().to_string_lossy() != manifest.name {
        return Ok(None);
    }
    let resources = resources(&digest_root, markdown.len() as u64, bytes_left)?;
    let mut tags = manifest
        .metadata
        .get("tags")
        .into_iter()
        .flat_map(|tags| tags.split(','))
        .map(|tag| tag.trim().to_lowercase())
        .filter(|tag| !tag.is_empty())
        .collect::<Vec<_>>();
    tags.sort();
    tags.dedup();
    let identifiers = manifest
        .name
        .split('-')
        .map(str::to_string)
        .collect::<Vec<_>>();
    Ok(Some(AgentSkillRecord {
        name: manifest.name,
        description: manifest.description,
        digest,
        tags,
        identifiers,
        skill_md_path: markdown_path,
        skill_md_bytes: markdown.len() as u64,
        skill_md_sha256: sha256_hex(&markdown),
        resources,
        allowed_tools: manifest.allowed_tools,
        learned_js: manifest.learned_js,
        embedding: Vec::new(),
    }))
}

fn select_active_digest(name_root: &Path) -> Result<Option<String>, CatalogError> {
    let pointer = name_root.join("ACTIVE");
    match fs::symlink_metadata(&pointer) {
        Ok(_) => {
            // An explicit but invalid pointer must never fall through to legacy selection.
            let bytes = super::import::read_stable_file(&pointer, MAX_ACTIVE_POINTER_BYTES, false)?;
            let digest = std::str::from_utf8(&bytes)
                .map_err(|_| CatalogError::InvalidDigest)?
                .trim()
                .to_string();
            validate_digest(&digest)?;
            return Ok(Some(digest));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut digests = bounded_directory_entries(name_root)?
        .into_iter()
        .filter_map(|entry| {
            entry
                .file_type()
                .ok()
                .filter(|kind| kind.is_dir())
                .map(|_| entry.file_name().to_string_lossy().to_string())
        })
        .filter(|digest| validate_digest(digest).is_ok())
        .collect::<Vec<_>>();
    digests.sort();
    // Never guess. Sorting the digests and taking the last one made the winner
    // an artefact of SHA-256 ordering, so re-importing an updated skill could
    // silently leave the older version active. Import writes the pointer; a
    // package with several digests and no pointer is omitted instead.
    match digests.len() {
        0 => Ok(None),
        1 => Ok(digests.pop()),
        count => Err(CatalogError::AmbiguousActiveDigest(
            name_root
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default(),
            count,
        )),
    }
}

fn validate_digest(digest: &str) -> Result<(), CatalogError> {
    if digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(CatalogError::InvalidDigest)
    }
}

fn resources(
    root: &Path,
    instruction_bytes: u64,
    remaining: &mut u64,
) -> Result<Vec<ResourceMetadata>, CatalogError> {
    fn visit(
        root: &Path,
        current: &Path,
        depth: usize,
        entries: &mut usize,
        bytes_left: &mut u64,
        remaining: &mut u64,
        output: &mut BTreeMap<String, ResourceMetadata>,
    ) -> Result<(), CatalogError> {
        for entry in fs::read_dir(current)? {
            let entry = entry?;
            *entries += 1;
            if depth + 1 > super::import::MAX_DEPTH || *entries > super::import::MAX_ENTRIES {
                return Err(CatalogError::ResourceLimit);
            }
            let path = entry.path();
            portable::ensure_no_link_traversal(root, &path)?;
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                visit(
                    root,
                    &path,
                    depth + 1,
                    entries,
                    bytes_left,
                    remaining,
                    output,
                )?;
            } else if metadata.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| CatalogError::InvalidDigest)?
                    .to_string_lossy()
                    .replace('\\', "/");
                if relative != "SKILL.md" {
                    if metadata.len() > *bytes_left || metadata.len() > *remaining {
                        return Err(CatalogError::ResourceLimit);
                    }
                    *remaining = remaining.saturating_sub(metadata.len());
                    let bytes = super::import::read_stable_file(
                        &path,
                        super::import::MAX_FILE_BYTES.min(*bytes_left),
                        false,
                    )?;
                    *bytes_left -= bytes.len() as u64;
                    output.insert(
                        relative.clone(),
                        ResourceMetadata {
                            relative_path: relative,
                            bytes: bytes.len() as u64,
                            sha256: sha256_hex(&bytes),
                        },
                    );
                }
            }
        }
        Ok(())
    }
    // The instruction bytes were already read and hashed, but count toward the same tree limit.
    let mut bytes_left = super::import::MAX_EXPANDED_BYTES
        .checked_sub(instruction_bytes)
        .ok_or(CatalogError::ResourceLimit)?;
    let mut entries = 0;
    let mut output = BTreeMap::new();
    visit(
        root,
        root,
        0,
        &mut entries,
        &mut bytes_left,
        remaining,
        &mut output,
    )?;
    Ok(output.into_values().collect())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod bounded_signature_tests {
    #[test]
    fn aggregate_signature_entries_are_capped_across_package_directories() {
        let root = crate::tests::OwnedTestDirectory::new("skill-signature-cap");
        for package in 0..5 {
            let dir = root.path().join(format!("package{package}"));
            std::fs::create_dir(&dir).unwrap();
            for version in 0..820 {
                std::fs::write(dir.join(format!("version{version}")), b"").unwrap();
            }
        }
        assert!(matches!(
            super::catalog_signature(root.path()),
            Err(super::CatalogError::ResourceLimit)
        ));
        std::fs::remove_dir_all(root.path().join("package4")).unwrap();
        assert_eq!(
            super::catalog_signature(root.path()).unwrap().entries.len(),
            3285
        );
    }
}
