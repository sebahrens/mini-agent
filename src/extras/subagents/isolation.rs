//! Opt-in git-worktree isolation for write-capable subagent children.
//!
//! A persona with `mode: write` and `isolation: worktree` runs each child in
//! its own git worktree and branch, created through the same
//! `git_worktree::create` path as `/worktree`. The child's workspace binding,
//! sandbox and permission checker are bound to that worktree only
//! ([`SubagentAuthorization::isolated_to`]); nothing is applied to the parent
//! checkout. When the child ends, the host (not the model) commits what it
//! left in the worktree and reports the branch, commits and diff stat. A child
//! that changed nothing has its worktree and branch removed. A child whose
//! future is dropped (cancellation, timeout) keeps its branch when it left
//! anything behind, and
//! the call's report says which of the two happened ([`AbandonedNotes`]).
//! Merging stays an explicit user action.

use std::path::PathBuf;
use std::sync::Arc;

use super::builder::SubagentAuthorization;

/// Host-written lines about isolated worktrees whose child was dropped before
/// it finished (call deadline, sibling stop, Esc, `/tasks cancel`, compaction).
/// The drop cannot return a value, so its cleanup records the outcome here and
/// the scheduler appends it to the call's report.
#[derive(Clone, Default)]
pub(crate) struct AbandonedNotes(Arc<AbandonedInner>);

#[derive(Default)]
struct AbandonedInner {
    pending: std::sync::atomic::AtomicUsize,
    lines: std::sync::Mutex<Vec<String>>,
    done: tokio::sync::Notify,
}

impl std::fmt::Debug for AbandonedNotes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AbandonedNotes")
    }
}

#[cfg_attr(not(feature = "git-worktree"), allow(dead_code))]
impl AbandonedNotes {
    /// A cleanup was started; [`Self::render`] waits for its [`Self::finish`].
    fn begin(&self) {
        self.0
            .pending
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    fn finish(&self, line: String) {
        self.0
            .lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(line);
        self.0
            .pending
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        self.0.done.notify_waiters();
    }

    /// The section to append to the call's report, once the cleanups started
    /// by dropped children have settled (bounded wait).
    pub(crate) async fn render(&self) -> Option<String> {
        let settled = async {
            loop {
                let notified = self.0.done.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.0.pending.load(std::sync::atomic::Ordering::Acquire) == 0 {
                    break;
                }
                notified.await;
            }
        };
        let _ = tokio::time::timeout(std::time::Duration::from_secs(20), settled).await;
        let lines = self.0.lines.lock().unwrap_or_else(|e| e.into_inner());
        if lines.is_empty() {
            return None;
        }
        let mut section = String::from("\n\n## Isolated worktrees of stopped children\n");
        for line in lines.iter() {
            section.push_str("- ");
            section.push_str(line);
            section.push('\n');
        }
        Some(section)
    }
}

/// Where and from what isolated children are created.
#[derive(Clone, Debug)]
#[cfg_attr(not(feature = "git-worktree"), allow(dead_code))]
pub(crate) struct IsolationPlan {
    /// The parent session's workspace; children branch from its `HEAD`.
    pub(crate) parent_root: PathBuf,
    /// Directory that receives the worktrees. `None` is the parent's directory
    /// (the same default as `/worktree`).
    pub(crate) base_dir: Option<PathBuf>,
    /// Commit-ish the child branches start from (`task` `base_ref`);
    /// `None` is the parent's `HEAD`.
    pub(crate) base_ref: Option<String>,
    /// Outcomes of worktrees whose child was dropped; shared by all children
    /// of the call.
    pub(crate) abandoned: AbandonedNotes,
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
    /// The child made no change, but removing its worktree failed; both are
    /// still on disk.
    UnchangedNotRemoved {
        branch: String,
        worktree: PathBuf,
        error: String,
    },
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
            Self::UnchangedNotRemoved {
                branch,
                worktree,
                error,
            } => format!(
                "\n\n## Isolated worktree\n- The child made no changes, but removing its worktree failed: {error}\n- Branch `{branch}` and worktree {} are still on disk; remove them with `git worktree remove` and `git branch -D`.",
                worktree.display()
            ),
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

/// Check a `task` `base_ref` before any git command: one commit-ish, not an
/// option, printable ASCII without whitespace, at most 256 bytes, no range.
pub(crate) fn validate_base_ref(base: &str) -> Result<(), String> {
    if base.is_empty()
        || base.len() > 256
        || base.starts_with('-')
        || base.contains("..")
        || !base.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(format!(
            "task: base_ref must name one commit (a branch, tag or commit id; no options, ranges or whitespace), got {base:?}"
        ));
    }
    Ok(())
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
    /// future was cancelled or timed out) keeps the branch when the child left
    /// edits or commits, removes the worktree and branch when it left nothing,
    /// and records which in the call's [`AbandonedNotes`].
    pub(crate) struct IsolatedWorktree {
        info: WorktreeInfo,
        base: String,
        armed: bool,
        notes: super::AbandonedNotes,
    }

    /// What became of an abandoned worktree, as one report line.
    async fn settle_abandoned(info: &WorktreeInfo, base: &str) -> String {
        let branch = &info.branch;
        if base.is_empty() {
            // The worktree was dropped before the child's authorization was
            // derived from it: no child ever ran in it.
            return match git_worktree::discard_worktree(info).await {
                Ok(()) => format!(
                    "`{branch}`: the child was stopped before it started; its worktree and branch were discarded."
                ),
                Err(error) => format!(
                    "`{branch}`: the child was stopped before it started, but removing its worktree at {} failed: {error}",
                    info.worktree_path.display()
                ),
            };
        }
        let message = format!("subagent {branch}: changes");
        let kept = async {
            git_worktree::commit_all_if_dirty(&info.worktree_path, &message).await?;
            git_worktree::branch_report(info, base).await
        }
        .await;
        match kept {
            Ok(report) if report.commits.is_empty() => {
                match git_worktree::discard_worktree(info).await {
                    Ok(()) => format!(
                        "`{branch}`: the child was stopped before it changed anything; its worktree and branch were discarded."
                    ),
                    Err(error) => format!(
                        "`{branch}`: the child was stopped before it changed anything, but removing its worktree at {} failed: {error}",
                        info.worktree_path.display()
                    ),
                }
            }
            Ok(report) => format!(
                "`{branch}`: the child was stopped; its work was kept as branch `{branch}` at {} ({} commit(s) after {base}). Review it and merge explicitly, or remove it with `git worktree remove` and `git branch -D`.",
                info.worktree_path.display(),
                report.commits.len()
            ),
            Err(error) => format!(
                "`{branch}`: the child was stopped and its worktree at {} was kept because its state could not be inspected ({error}); nothing was discarded.",
                info.worktree_path.display()
            ),
        }
    }

    impl Drop for IsolatedWorktree {
        fn drop(&mut self) {
            if !self.armed {
                return;
            }
            let info = self.info.clone();
            let base = self.base.clone();
            let notes = self.notes.clone();
            // Registered with the owning agent scope when there is one, so
            // retirement waits for the settlement.
            if tokio::runtime::Handle::try_current().is_ok() {
                notes.begin();
                drop(crate::agent::runner::spawn_async_scoped(async move {
                    let line = settle_abandoned(&info, &base).await;
                    tracing::info!(%line, "subagent isolation: abandoned worktree settled");
                    notes.finish(line);
                }));
            } else {
                tracing::warn!(
                    worktree = %self.info.worktree_path.display(),
                    "subagent isolation: no runtime to settle the abandoned worktree; it was kept"
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

        async fn discard(mut self) -> Result<(), String> {
            self.armed = false;
            let result = git_worktree::discard_worktree(&self.info).await;
            if let Err(error) = &result {
                tracing::warn!(%error, "subagent isolation: worktree removal failed");
            }
            result
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
                Ok(report) if report.commits.is_empty() => match self.discard().await {
                    Ok(()) => IsolationOutcome::Unchanged,
                    Err(error) => IsolationOutcome::UnchangedNotRemoved {
                        branch,
                        worktree,
                        error,
                    },
                },
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
            let created = match create_plan.base_ref.as_deref() {
                Some(base) => {
                    git_worktree::create_at(
                        &create_plan.parent_root,
                        &name,
                        create_plan.base_dir.as_deref(),
                        base,
                    )
                    .await
                }
                None => {
                    git_worktree::create(
                        &create_plan.parent_root,
                        &name,
                        create_plan.base_dir.as_deref(),
                    )
                    .await
                }
            }
            .map(|(_, info)| IsolatedWorktree {
                info,
                base: String::new(),
                armed: true,
                notes: create_plan.abandoned.clone(),
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
                let _ = owned.discard().await;
                Err(error)
            }
        }
    }
}
