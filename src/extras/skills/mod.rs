//! Validated import of portable, instruction-only Agent Skills.
//!
//! Imported resources remain inert files. In particular, `allowed-tools` is
//! descriptive metadata and bundled JavaScript is never admitted to the
//! learned-JS store by this module. A bounded `learned-js` frontmatter list may
//! associate exact, separately verified identities; it never changes their
//! approval or activation state.
//!
//! Every item here is gated behind the `skills` feature: the catalog, index and
//! loader that read an installed tree only exist in a `skills` build, so an
//! import in any other build would install a tree that nothing ever reads.

/// Largest `SKILL.md` an Agent Skill may install with.
///
/// One turn's whole instruction budget in [`index::AgentSkillSearchPolicy`] is
/// this many bytes, so a larger `SKILL.md` would be embedded, ranked first and
/// then dropped from every turn. Import refuses it instead, and the catalog
/// omits any tree already installed above it.
#[cfg(feature = "skills")]
pub const MAX_SKILL_INSTRUCTION_BYTES: u64 = 48 * 1024;

#[cfg(feature = "skills")]
pub mod catalog;
#[cfg(feature = "skills")]
mod import;
#[cfg(feature = "skills")]
pub mod index;
#[cfg(feature = "skills")]
pub mod loader;
// `manifest` parses the whole documented frontmatter surface, including the
// `license` and `compatibility` fields no consumer reads yet. The allow is
// scoped to that one module instead of covering all of `extras::skills`; the
// module-wide allow it replaces was hiding unread fields in every sibling.
#[cfg(feature = "skills")]
#[allow(dead_code)]
mod manifest;

#[cfg(feature = "skills")]
#[allow(unused_imports)]
pub use import::{ImportError, ImportedSkill, TreeIdentity, import_agent_skill};
#[cfg(feature = "skills")]
#[allow(unused_imports)]
pub use manifest::{AgentSkillManifest, ManifestError};
