//! Structured Git support shared by internal worktree operations and the
//! model-visible typed Git tool.

pub(crate) mod runner;
#[cfg(any(test, feature = "git-worktree"))]
pub(crate) mod tool;

pub(crate) mod global_snapshot;
pub(crate) mod runtime;
