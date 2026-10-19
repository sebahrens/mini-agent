//! Opt-in git-worktree isolation for write-capable subagent children.
//!
//! A persona with `mode: write` and `isolation: worktree` runs each child in
//! its own git worktree and branch, created through the same
//! `git_worktree::create` path as `/worktree`. The child's workspace binding,
//! sandbox and permission checker are bound to that worktree only
//! ([`SubagentAuthorization::isolated_to`]); nothing is applied to the parent
//! checkout. When the child ends, the host (not the model) commits what it
//! left in the worktree and reports the branch, commits and diff stat. A child
//! that changed nothing, or whose future is dropped (cancellation, timeout),
//! has its worktree and branch removed. Merging stays an explicit user action.

use std::path::PathBuf;

use super::builder::SubagentAuthorization;

/// Where and from what isolated children are created.
#[derive(Clone, Debug)]
#[cfg_attr(not(feature = "git-worktree"), allow(dead_code))]
pub(crate) struct IsolationPlan {
    /// The parent session's workspace; children branch from its `HEAD`.
    pub(crate) parent_root: PathBuf,
    /// Directory that receives the worktrees. `None` is the parent's directory
    /// (the same default as `/worktree`).
    pub(crate) base_dir: Option<PathBuf>,
    /// Test seam: signalled once creation finished, then held until released,
    /// so a test can drop the caller while the create is still in flight.
    #[cfg(test)]
    pub(crate) create_gate: Option<(
        std::sync::Arc<tokio::sync::Notify>,
        std::sync::Arc<tokio::sync::Semaphore>,
    )>,
}

/// What an isolated child left behind.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(feature = "git-worktree"), allow(dead_code))]
pub(crate) enum IsolationOutcome {
    /// The child made no change; worktree and branch were removed.
    Unchanged,
    /// The child's changes are committed on `branch` in `worktree`.
    Branch {
        branch: String,
        worktree: PathBuf,
        base: String,
        commits: Vec<String>,
        diff_stat: String,
    },
    /// Collecting the result failed; the worktree is kept for inspection.
    Retained {
        branch: String,
        worktree: PathBuf,
        error: String,
    },
}

impl IsolationOutcome {
    /// Host-written section appended to the child's report. It is outside the
    /// model-authored text, so the child cannot forge or suppress it.
    pub(crate) fn render(&self) -> String {
        match self {
            Self::Unchanged => "\n\n## Isolated worktree\n- The child made no changes; its worktree and branch were removed.".to_string(),
            Self::Branch {
                branch,
                worktree,
                base,
                commits,
                diff_stat,
            } => {
                let commits = commits
                    .iter()
                    .map(|commit| format!("  - {commit}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    "\n\n## Isolated worktree\n- Branch: `{branch}`\n- Worktree: {}\n- Base commit: {base}\n- Commits:\n{commits}\n- Diff stat:\n```\n{diff_stat}\n```\nNothing was applied to the parent workspace. Review the branch, then merge it explicitly (for example `git merge {branch}` from the parent, or switch with /worktree and use /wt-merge).",
                    worktree.display()
                )
            }
            Self::Retained {
                branch,
                worktree,
                error,
            } => format!(
                "\n\n## Isolated worktree\n- Branch `{branch}` at {} was kept, but collecting its result failed: {error}\nNothing was applied to the parent workspace.",
                worktree.display()
            ),
        }
    }
}

#[cfg(feature = "git-worktree")]
pub(crate) use real::prepare;
#[cfg(not(feature = "git-worktree"))]
pub(crate) use stub::prepare;

#[cfg(not(feature = "git-worktree"))]
mod stub {
    use super::{IsolationOutcome, IsolationPlan, SubagentAuthorization};

    pub(crate) struct IsolatedWorktree;

    impl IsolatedWorktree {
        pub(crate) async fn finish(self) -> IsolationOutcome {
            IsolationOutcome::Unchanged
        }
    }

    pub(crate) async fn prepare(
        _plan: &IsolationPlan,
        _authorization: &SubagentAuthorization,
    ) -> Result<(IsolatedWorktree, SubagentAuthorization), String> {
        Err("worktree isolation requires a build with the git-worktree feature".into())
    }
}

#[cfg(feature = "git-worktree")]
mod real {
    use std::sync::Arc;

    use super::{IsolationOutcome, IsolationPlan, SubagentAuthorization};
    use crate::extras::git_worktree::{self, WorktreeInfo};

    /// A worktree owned by one child. Dropping it while still armed (the child
    /// future was cancelled or timed out) removes the worktree and branch.
    pub(crate) struct IsolatedWorktree {
        info: WorktreeInfo,
        base: String,
        armed: bool,
    }

    impl Drop for IsolatedWorktree {
        fn drop(&mut self) {
            if !self.armed {
                return;
            }
            let info = self.info.clone();
            let cleanup = async move {
                if let Err(error) = git_worktree::discard_worktree(&info).await {
                    tracing::warn!(
                        %error,
                        branch = %info.branch,
                        worktree = %info.worktree_path.display(),
                        "subagent isolation: could not remove the abandoned worktree"
                    );
                }
            };
            // Registered with the owning agent scope when there is one, so
            // retirement waits for the removal.
            if tokio::runtime::Handle::try_current().is_ok() {
                drop(crate::agent::runner::spawn_async_scoped(cleanup));
            } else {
                tracing::warn!(
                    worktree = %self.info.worktree_path.display(),
                    "subagent isolation: no runtime to remove the abandoned worktree"
                );
            }
        }
    }

    impl IsolatedWorktree {
        #[cfg(test)]
        pub(crate) fn path(&self) -> &std::path::Path {
            &self.info.worktree_path
        }

        #[cfg(test)]
        pub(crate) fn branch(&self) -> &str {
            &self.info.branch
        }

        async fn discard(mut self) {
            self.armed = false;
            if let Err(error) = git_worktree::discard_worktree(&self.info).await {
                tracing::warn!(%error, "subagent isolation: worktree removal failed");
            }
        }

        /// Commit whatever the child left, then report or remove the branch.
        pub(crate) async fn finish(mut self) -> IsolationOutcome {
            let branch = self.info.branch.clone();
            let worktree = self.info.worktree_path.clone();
            let message = format!("subagent {branch}: changes");
            let collected = async {
                git_worktree::commit_all_if_dirty(&worktree, &message).await?;
                git_worktree::branch_report(&self.info, &self.base).await
            }
            .await;
            match collected {
                Ok(report) if report.commits.is_empty() => {
                    self.discard().await;
                    IsolationOutcome::Unchanged
                }
                Ok(report) => {
                    self.armed = false;
                    IsolationOutcome::Branch {
                        branch,
                        worktree,
                        base: self.base.clone(),
                        commits: report.commits,
                        diff_stat: report.diff_stat,
                    }
                }
                Err(error) => {
                    self.armed = false;
                    IsolationOutcome::Retained {
                        branch,
                        worktree,
                        error,
                    }
                }
            }
        }
    }

    /// Create the child's worktree and derive its authorization. On any
    /// failure nothing is left behind.
    pub(crate) async fn prepare(
        plan: &IsolationPlan,
        parent: &SubagentAuthorization,
    ) -> Result<(IsolatedWorktree, SubagentAuthorization), String> {
        if let Some(base) = &plan.base_dir {
            std::fs::create_dir_all(base)
                .map_err(|error| format!("cannot create {}: {error}", base.display()))?;
        }
        let name = format!(
            "subagent-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        );
        // Creation runs in its own task and hands back an armed guard through
        // a channel. If this future is dropped first, the guard is dropped
        // wherever it ends up (failed send, or the channel's own drop) and
        // discards the worktree, so a late-finishing create is never orphaned.
        let (tx, rx) = tokio::sync::oneshot::channel();
        let create_plan = plan.clone();
        drop(crate::agent::runner::spawn_async_scoped(async move {
            let created = git_worktree::create(
                &create_plan.parent_root,
                &name,
                create_plan.base_dir.as_deref(),
            )
            .await
            .map(|(_, info)| IsolatedWorktree {
                info,
                base: String::new(),
                armed: true,
            });
            #[cfg(test)]
            if let Some((entered, release)) = &create_plan.create_gate {
                entered.notify_one();
                if let Ok(permit) = release.acquire().await {
                    permit.forget();
                }
            }
            let _ = tx.send(created);
        }));
        let mut owned = rx
            .await
            .map_err(|_| "worktree creation task stopped before returning".to_string())??;
        let derived = async {
            owned.base = git_worktree::head_oid(&owned.info.worktree_path).await?;
            let binding = crate::paths::WorkspaceBinding::capture(&owned.info.worktree_path)
                .map_err(|error| format!("cannot bind the isolated worktree: {error}"))?;
            let denied = [
                plan.parent_root
                    .canonicalize()
                    .unwrap_or_else(|_| plan.parent_root.clone()),
                owned.info.main_repo_path.clone(),
            ];
            parent.isolated_to(Arc::new(binding), &denied)
        }
        .await;
        match derived {
            Ok(authorization) => Ok((owned, authorization)),
            Err(error) => {
                owned.discard().await;
                Err(error)
            }
        }
    }
}
