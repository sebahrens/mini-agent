//! Immutable metadata catalog for installed instruction-only Agent Skills.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use crate::extras::js::skills::embed::Embedder;
use crate::paths::{AppPaths, portable};

use super::index::{AgentSkillIndex, AgentSkillRecord};
use super::manifest::parse_skill_markdown;

// A tree whose SKILL.md is larger than one turn's instruction budget can never
// be surfaced, so it is omitted from the generation rather than ranked first
// and then dropped. Import refuses to install one above this bound.
const MAX_SKILL_MD_BYTES: u64 = super::MAX_SKILL_INSTRUCTION_BYTES;
const MAX_RESOURCES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceMetadata {
    pub relative_path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Manifest(#[from] super::ManifestError),
    #[error(transparent)]
    Portable(#[from] portable::PortablePathError),
    #[error(transparent)]
    Embedding(#[from] crate::extras::js::skills::embed::EmbeddingError),
    #[error(transparent)]
    Index(#[from] super::index::AgentSkillIndexError),
    #[error("installed Agent Skill catalog contains an invalid digest path")]
    InvalidDigest,
    #[error("Agent Skill catalog exceeds its bounded resource count")]
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
}

// A signature is only ever compared as a whole. The derived `PartialEq` reads
// every field, but rustc does not count a derived impl as a read, so the allow
// is scoped to these two types instead of the whole `extras::skills` module.
#[derive(Clone, Eq, PartialEq)]
#[allow(dead_code)]
struct CatalogSignature {
    entries: Vec<CatalogSignatureEntry>,
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

    /// Scan installed immutable trees, batch metadata embeddings, and construct one generation.
    pub fn refresh(&mut self, embedder: &Embedder) -> Result<AgentSkillIndex, CatalogError> {
        // Capture the tree before scanning. An import racing this build changes
        // the next turn's signature and therefore forces another refresh.
        let signature = catalog_signature(&self.root)?;
        let mut records = Vec::new();
        if self.root.is_dir() {
            let mut names = fs::read_dir(&self.root)?.collect::<Result<Vec<_>, _>>()?;
            names.sort_by_key(|entry| entry.file_name());
            for name_entry in names {
                if !name_entry.file_type()?.is_dir() {
                    continue;
                }
                // Installed packages are independent trust domains. A corrupt package is
                // omitted from this generation without making valid siblings unavailable.
                if let Ok(Some(record)) = scan_record(&self.root, &name_entry) {
                    records.push(record);
                }
            }
        }
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

    /// Rebuild only after an import or ACTIVE-pointer change is visible.
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
    .then(|| fs::read(&path).map(|bytes| sha256_hex(&bytes)))
    .transpose()?;
    Ok(CatalogSignatureEntry {
        path,
        bytes: metadata.len(),
        modified: metadata.modified().ok(),
        kind,
        pointer_digest,
    })
}

fn catalog_signature(root: &Path) -> Result<CatalogSignature, CatalogError> {
    if !root.exists() {
        return Ok(CatalogSignature {
            entries: Vec::new(),
        });
    }
    let mut entries = vec![signature_entry(root.to_path_buf())?];
    let mut names = fs::read_dir(root)?.collect::<Result<Vec<_>, _>>()?;
    names.sort_by_key(|entry| entry.file_name());
    for name in names {
        entries.push(signature_entry(name.path())?);
        if name.file_type()?.is_dir() {
            let mut children = fs::read_dir(name.path())?.collect::<Result<Vec<_>, _>>()?;
            children.sort_by_key(|entry| entry.file_name());
            for child in children {
                entries.push(signature_entry(child.path())?);
            }
        }
    }
    Ok(CatalogSignature { entries })
}

fn scan_record(
    root: &Path,
    name_entry: &fs::DirEntry,
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
    if !metadata.is_file() || metadata.len() > MAX_SKILL_MD_BYTES {
        return Ok(None);
    }
    let markdown = fs::read(&markdown_path)?;
    let manifest = parse_skill_markdown(&markdown)?;
    if name_entry.file_name().to_string_lossy() != manifest.name {
        return Ok(None);
    }
    let resources = resources(&digest_root)?;
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
        skill_md_bytes: metadata.len(),
        skill_md_sha256: sha256_hex(&markdown),
        resources,
        allowed_tools: manifest.allowed_tools,
        learned_js: manifest.learned_js,
        embedding: Vec::new(),
    }))
}

fn select_active_digest(name_root: &Path) -> Result<Option<String>, CatalogError> {
    let pointer = name_root.join("ACTIVE");
    if pointer.is_file() {
        let digest = fs::read_to_string(pointer)?.trim().to_string();
        validate_digest(&digest)?;
        return Ok(Some(digest));
    }
    let mut digests = fs::read_dir(name_root)?
        .filter_map(Result::ok)
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

fn resources(root: &Path) -> Result<Vec<ResourceMetadata>, CatalogError> {
    fn visit(
        root: &Path,
        current: &Path,
        output: &mut BTreeMap<String, ResourceMetadata>,
    ) -> Result<(), CatalogError> {
        for entry in fs::read_dir(current)? {
            let entry = entry?;
            let path = entry.path();
            portable::ensure_no_link_traversal(root, &path)?;
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                visit(root, &path, output)?;
            } else if metadata.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| CatalogError::InvalidDigest)?
                    .to_string_lossy()
                    .replace('\\', "/");
                if relative != "SKILL.md" {
                    let bytes = fs::read(&path)?;
                    output.insert(
                        relative.clone(),
                        ResourceMetadata {
                            relative_path: relative,
                            bytes: metadata.len(),
                            sha256: sha256_hex(&bytes),
                        },
                    );
                }
            }
            if output.len() > MAX_RESOURCES {
                return Err(CatalogError::ResourceLimit);
            }
        }
        Ok(())
    }
    let mut output = BTreeMap::new();
    visit(root, root, &mut output)?;
    Ok(output.into_values().collect())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
