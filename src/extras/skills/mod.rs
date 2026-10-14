//! Validated import of portable, instruction-only Agent Skills.
//!
//! Imported resources remain inert files. In particular, `allowed-tools` is
//! descriptive metadata and bundled JavaScript is never admitted to the
//! learned-JS store by this module. A bounded `learned-js` frontmatter list may
//! associate exact, separately verified identities; it never changes their
//! approval or activation state.
//!
//! Every item here is gated behind the `agent-skills` feature: the catalog, index and
//! loader that read an installed tree only exist in an `agent-skills` build, so an
//! import in any other build would install a tree that nothing ever reads.

/// Largest `SKILL.md` an Agent Skill may install with.
///
/// One turn's whole instruction budget in [`index::AgentSkillSearchPolicy`] is
/// this many bytes, so a larger `SKILL.md` would be embedded, ranked first and
/// then dropped from every turn. Import refuses it instead, and the catalog
/// omits any tree already installed above it.
#[cfg(feature = "agent-skills")]
pub const MAX_SKILL_INSTRUCTION_BYTES: u64 = 48 * 1024;

#[cfg(feature = "agent-skills")]
pub mod catalog;
#[cfg(feature = "agent-skills")]
mod import;
#[cfg(feature = "agent-skills")]
pub mod index;
#[cfg(feature = "agent-skills")]
pub mod loader;
#[cfg(feature = "agent-skills")]
mod retention;
// `manifest` parses the whole documented frontmatter surface, including the
// `license` and `compatibility` fields no consumer reads yet. The allow is
// scoped to that one module instead of covering all of `extras::skills`; the
// module-wide allow it replaces was hiding unread fields in every sibling.
#[cfg(feature = "agent-skills")]
#[allow(dead_code)]
mod manifest;

#[cfg(feature = "agent-skills")]
#[allow(unused_imports)]
pub use import::{ImportError, ImportedSkill, TreeIdentity, import_agent_skill};
#[cfg(feature = "agent-skills")]
#[allow(unused_imports)]
pub use manifest::{AgentSkillManifest, ManifestError};

#[cfg(feature = "agent-skills")]
pub(crate) mod retrieval;

#[cfg(feature = "agent-skills")]
pub(crate) mod discovery;

#[cfg(feature = "agent-skills")]
pub(crate) mod selection;

#[cfg(feature = "agent-skills")]
pub(crate) mod session;

#[cfg(feature = "agent-skills")]
pub(crate) mod search_tool;
