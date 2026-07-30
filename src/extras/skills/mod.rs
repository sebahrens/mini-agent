//! Validated import of portable, instruction-only Agent Skills.
//!
//! Imported resources remain inert files. In particular, `allowed-tools` is
//! descriptive metadata and bundled JavaScript is never admitted to the
//! learned-JS store by this module.

mod import;
mod manifest;

#[allow(unused_imports)]
pub use import::{ImportError, ImportedSkill, TreeIdentity, import_agent_skill};
#[allow(unused_imports)]
pub use manifest::{AgentSkillManifest, ManifestError};
