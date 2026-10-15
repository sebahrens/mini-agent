//! Adaptive schedule for the TUI's Git branch and status refresh.
//!
//! The event loop used to read `.git/HEAD` on the UI task and spawn
//! `git status` once per second for the whole lifetime of the TUI. The
//! schedule here refreshes right after events that can change the repository
//! (tool results, file mutations, turn end, submitted commands, resume from a
//! suspension) and otherwise every [`GIT_POLL_INTERVAL`]. Before spawning Git
//! the refresh task compares a cheap metadata [`GitFingerprint`] of the
//! repository and skips the spawn when nothing changed, except that a full
//! status is forced at least every [`GIT_POLL_MAX_STALENESS`] so edits made
//! outside the agent (which never touch `.git`) still appear.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// Background cadence while nothing that can change Git state happened.
pub(crate) const GIT_POLL_INTERVAL: Duration = Duration::from_secs(10);
/// Upper bound on how long a fingerprint match may suppress `git status`.
pub(crate) const GIT_POLL_MAX_STALENESS: Duration = Duration::from_secs(30);
/// Minimum spacing between refreshes requested by repository-changing events,
/// so a burst of tool results costs at most one spawn per second.
pub(crate) const GIT_POLL_MIN_SPACING: Duration = Duration::from_secs(1);

/// Scheduling state; owned by the event loop.
#[derive(Debug, Default)]
pub(crate) struct GitPoll {
    /// `None` means due now (startup, or a forced refresh after its spacing).
    next_due: Option<Instant>,
    /// A repository-changing event happened since the last dispatch.
    forced: bool,
    last_start: Option<Instant>,
    last_full: Option<Instant>,
    /// Fingerprint observed by the last refresh, with the root it describes.
    fingerprint: Option<(PathBuf, GitFingerprint)>,
    was_running: bool,
    #[cfg(test)]
    pub(crate) dispatches: usize,
}

/// What one dispatched refresh must do.
#[derive(Debug, Clone)]
pub(crate) struct GitPollRequest {
    /// Run `git status` even when the fingerprint is unchanged.
    pub(crate) force: bool,
    pub(crate) previous: Option<(PathBuf, GitFingerprint)>,
}

impl GitPollRequest {
    /// Whether a refresh that observed `current` for `root` may skip the
    /// `git status` spawn.
    pub(crate) fn may_skip_status(&self, root: &Path, current: Option<&GitFingerprint>) -> bool {
        !self.force
            && current.is_some_and(|current| {
                self.previous
                    .as_ref()
                    .is_some_and(|(previous_root, previous)| {
                        previous_root == root && previous == current
                    })
            })
    }
}

impl GitPoll {
    /// Something that can change the repository happened: refresh as soon as
    /// the minimum spacing since the previous dispatch allows.
    pub(crate) fn mark_changed(&mut self, now: Instant) {
        self.forced = true;
        let earliest = self
            .last_start
            .map_or(now, |start| (start + GIT_POLL_MIN_SPACING).max(now));
        self.next_due = Some(self.next_due.map_or(earliest, |due| due.min(earliest)));
    }

    /// Track run transitions: the end of a turn (including an interrupted
    /// one) is a repository-changing event.
    pub(crate) fn observe_running(&mut self, running: bool, now: Instant) {
        if self.was_running && !running {
            self.mark_changed(now);
        }
        self.was_running = running;
    }

    /// A dispatched refresh turned out stale (the workspace or session
    /// directory changed meanwhile): refresh again at once.
    pub(crate) fn retry_now(&mut self) {
        self.forced = true;
        self.next_due = None;
    }

    /// When a pending forced refresh becomes due, so an otherwise idle loop
    /// wakes for it. Background polls never wake the loop on their own.
    pub(crate) fn forced_deadline(&self) -> Option<Instant> {
        if self.forced { self.next_due } else { None }
    }

    pub(crate) fn due(&self, now: Instant) -> bool {
        self.next_due.is_none_or(|due| now >= due)
    }

    /// A refresh is being dispatched now.
    pub(crate) fn start(&mut self, now: Instant) -> GitPollRequest {
        let stale = self
            .last_full
            .is_none_or(|full| now.duration_since(full) >= GIT_POLL_MAX_STALENESS);
        let request = GitPollRequest {
            force: self.forced || stale,
            previous: self.fingerprint.clone(),
        };
        self.forced = false;
        self.last_start = Some(now);
        self.next_due = Some(now + GIT_POLL_INTERVAL);
        #[cfg(test)]
        {
            self.dispatches += 1;
        }
        request
    }

    /// Record a completed refresh.
    pub(crate) fn finished(
        &mut self,
        now: Instant,
        root: PathBuf,
        fingerprint: Option<GitFingerprint>,
        ran_status: bool,
    ) {
        if ran_status {
            self.last_full = Some(now);
        }
        self.fingerprint = fingerprint.map(|fingerprint| (root, fingerprint));
    }
}

/// Modification time and length of each repository file whose change can
/// alter `git status --branch` output or the branch name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitFingerprint(Vec<Option<(SystemTime, u64)>>);

fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

/// Resolve the Git directory of `root` (following a worktree/submodule
/// `.git` file) and its common directory.
fn git_dirs(root: &Path) -> Option<(PathBuf, PathBuf)> {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else if dot_git.is_file() {
        let content = std::fs::read_to_string(&dot_git).ok()?;
        let relative = PathBuf::from(content.strip_prefix("gitdir:")?.trim());
        if relative.is_absolute() {
            relative
        } else {
            root.join(relative)
        }
    } else {
        return None;
    };
    let common = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(text) => {
            let path = PathBuf::from(text.trim());
            if path.is_absolute() {
                path
            } else {
                git_dir.join(path)
            }
        }
        Err(_) => git_dir.clone(),
    };
    Some((git_dir, common))
}

/// Cheap metadata fingerprint of the repository at `root`, or `None` outside
/// a repository. Covers HEAD, the index, the current branch's ref (loose ref
/// files are replaced by rename, so its stamp changes on every commit), the
/// ref directories, packed refs and the last fetch.
pub(crate) fn fingerprint(root: &Path) -> Option<GitFingerprint> {
    let (git_dir, common) = git_dirs(root)?;
    let head_path = git_dir.join("HEAD");
    let head = std::fs::read_to_string(&head_path).ok();
    let mut stamps = vec![
        stamp(&head_path),
        stamp(&git_dir.join("index")),
        stamp(&common.join("packed-refs")),
        stamp(&common.join("FETCH_HEAD")),
        stamp(&common.join("refs").join("heads")),
        stamp(&common.join("refs").join("remotes")),
    ];
    if let Some(reference) = head
        .as_deref()
        .and_then(|head| head.trim().strip_prefix("ref:"))
        .map(str::trim)
        .filter(|reference| {
            reference.starts_with("refs/")
                && !reference
                    .split('/')
                    .any(|part| part == ".." || part.is_empty())
        })
    {
        stamps.push(stamp(&common.join(reference)));
    }
    Some(GitFingerprint(stamps))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_polls_once_per_interval_and_changes_refresh_after_min_spacing() {
        let start = Instant::now();
        let mut poll = GitPoll::default();
        assert!(poll.due(start), "the first refresh runs at once");
        let first = poll.start(start);
        assert!(first.force, "nothing observed yet: run git status");
        // Event-loop iterations within the interval never dispatch again.
        for ms in [1, 100, 1_000, 9_999] {
            assert!(!poll.due(start + Duration::from_millis(ms)));
        }
        assert!(poll.due(start + GIT_POLL_INTERVAL));
        // A tool result right after a dispatch waits only the spacing.
        poll.mark_changed(start + Duration::from_millis(10));
        assert!(!poll.due(start + Duration::from_millis(500)));
        assert!(poll.due(start + GIT_POLL_MIN_SPACING));
        // An idle loop wakes for that forced refresh, never for idle polls.
        assert_eq!(poll.forced_deadline(), Some(start + GIT_POLL_MIN_SPACING));
        let forced = poll.start(start + GIT_POLL_MIN_SPACING);
        assert!(forced.force);
        assert_eq!(poll.dispatches, 2);
        assert_eq!(poll.forced_deadline(), None);
        // A stale result retries at once, regardless of the spacing.
        poll.retry_now();
        assert!(poll.due(start + GIT_POLL_MIN_SPACING));
        poll.start(start + GIT_POLL_MIN_SPACING);
        // A change long after the last dispatch refreshes immediately.
        let later = start + Duration::from_secs(5);
        poll.mark_changed(later);
        assert!(poll.due(later));
    }

    #[test]
    fn turn_end_marks_the_repository_changed() {
        let now = Instant::now();
        let mut poll = GitPoll::default();
        poll.start(now);
        poll.observe_running(true, now);
        assert!(!poll.due(now + Duration::from_secs(2)));
        poll.observe_running(false, now + Duration::from_secs(2));
        assert!(poll.due(now + Duration::from_secs(2)));
        assert!(poll.start(now + Duration::from_secs(2)).force);
    }

    #[test]
    fn unchanged_fingerprint_skips_status_until_stale() {
        let root = crate::tests::OwnedTestDirectory::new("git-poll-fingerprint");
        let git = root.path().join(".git");
        std::fs::create_dir_all(git.join("refs").join("heads")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(git.join("refs").join("heads").join("main"), "1\n").unwrap();
        std::fs::write(git.join("index"), "index-1").unwrap();
        let observed = fingerprint(root.path()).expect("repository fingerprint");
        assert_eq!(fingerprint(root.path()), Some(observed.clone()));

        let start = Instant::now();
        let mut poll = GitPoll::default();
        let first = poll.start(start);
        assert!(!first.may_skip_status(root.path(), Some(&observed)));
        poll.finished(
            start,
            root.path().to_path_buf(),
            Some(observed.clone()),
            true,
        );
        let idle = poll.start(start + GIT_POLL_INTERVAL);
        assert!(idle.may_skip_status(root.path(), Some(&observed)));
        // Another root, or no repository, never reuses the observation.
        assert!(!idle.may_skip_status(Path::new("/elsewhere"), Some(&observed)));
        assert!(!idle.may_skip_status(root.path(), None));
        // Staleness forces a real status even with an identical fingerprint.
        let stale = poll.start(start + GIT_POLL_MAX_STALENESS);
        assert!(!stale.may_skip_status(root.path(), Some(&observed)));

        // A changed index or branch ref changes the fingerprint.
        std::fs::write(git.join("index"), "index-two").unwrap();
        assert_ne!(fingerprint(root.path()), Some(observed.clone()));
        let after_index = fingerprint(root.path()).unwrap();
        std::fs::write(git.join("refs").join("heads").join("main"), "22\n").unwrap();
        assert_ne!(fingerprint(root.path()), Some(after_index));
        assert_eq!(fingerprint(&root.path().join("missing")), None);
    }

    #[test]
    fn worktree_git_file_resolves_its_common_directory() {
        let root = crate::tests::OwnedTestDirectory::new("git-poll-worktree");
        let common = root.path().join("main").join(".git");
        let linked = common.join("worktrees").join("feature");
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::create_dir_all(common.join("refs").join("heads")).unwrap();
        std::fs::write(linked.join("HEAD"), "ref: refs/heads/feature\n").unwrap();
        std::fs::write(linked.join("commondir"), "../..\n").unwrap();
        let tree = root.path().join("feature");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join(".git"), format!("gitdir: {}\n", linked.display())).unwrap();
        let before = fingerprint(&tree).expect("linked worktree fingerprint");
        std::fs::write(common.join("refs").join("heads").join("feature"), "1\n").unwrap();
        assert_ne!(fingerprint(&tree), Some(before));
    }
}
