//! Validated import of portable, instruction-only Agent Skills.
//!
//! Imported resources remain inert files. In particular, `allowed-tools` is
//! descriptive metadata and bundled JavaScript is never admitted to the
//! learned-JS store by this module.

// Phase 3 exposes catalog lifecycle fields that Phase 4 admission consumes.
#![allow(dead_code)]

#[cfg(feature = "skills")]
pub mod catalog;
mod import;
#[cfg(feature = "skills")]
pub mod index;
#[cfg(feature = "skills")]
pub mod loader;
mod manifest;

#[allow(unused_imports)]
pub use import::{ImportError, ImportedSkill, TreeIdentity, import_agent_skill};
#[allow(unused_imports)]
pub use manifest::{AgentSkillManifest, ManifestError};
