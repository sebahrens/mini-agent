//! Bounded, containment-rechecked progressive loading of selected Agent Skill content.

use std::fs;

use crate::paths::portable;
use sha2::{Digest, Sha256};

use super::index::AgentSkillRecord;

const MAX_SKILL_MD_BYTES: u64 = 256 * 1024;
const MAX_RESOURCE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Portable(#[from] portable::PortablePathError),
    #[error("selected Agent Skill content is not valid UTF-8")]
    NonUtf8,
    #[error("selected Agent Skill content exceeds its byte limit")]
    TooLarge,
    #[error("resource was not present in the immutable selected manifest")]
    UnknownResource,
    #[error("selected Agent Skill content changed after catalog publication")]
    ContentChanged,
}

pub fn load_skill_markdown(record: &AgentSkillRecord) -> Result<String, LoadError> {
    let root = record
        .skill_md_path
        .parent()
        .ok_or(LoadError::UnknownResource)?;
    portable::ensure_no_link_traversal(root, &record.skill_md_path)?;
    let metadata = fs::symlink_metadata(&record.skill_md_path)?;
    if !metadata.is_file()
        || metadata.len() != record.skill_md_bytes
        || metadata.len() > MAX_SKILL_MD_BYTES
    {
        return Err(LoadError::TooLarge);
    }
    let bytes = fs::read(&record.skill_md_path)?;
    verify_sha256(&bytes, &record.skill_md_sha256)?;
    String::from_utf8(bytes).map_err(|_| LoadError::NonUtf8)
}

pub fn load_resource(record: &AgentSkillRecord, relative_path: &str) -> Result<Vec<u8>, LoadError> {
    let resource = record
        .resources
        .iter()
        .find(|resource| resource.relative_path == relative_path)
        .ok_or(LoadError::UnknownResource)?;
    if resource.bytes > MAX_RESOURCE_BYTES {
        return Err(LoadError::TooLarge);
    }
    let root = record
        .skill_md_path
        .parent()
        .ok_or(LoadError::UnknownResource)?;
    let path = root.join(relative_path);
    portable::ensure_no_link_traversal(root, &path)?;
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.is_file()
        || metadata.len() != resource.bytes
        || metadata.len() > MAX_RESOURCE_BYTES
    {
        return Err(LoadError::TooLarge);
    }
    let bytes = fs::read(path)?;
    verify_sha256(&bytes, &resource.sha256)?;
    Ok(bytes)
}

fn verify_sha256(bytes: &[u8], expected: &str) -> Result<(), LoadError> {
    let actual = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if actual == expected {
        Ok(())
    } else {
        Err(LoadError::ContentChanged)
    }
}
