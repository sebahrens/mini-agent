//! Cross-process ownership of saved sessions (mini-agent-e42za).
//!
//! `save_session` replaces the whole `<id>.json` snapshot, so two processes
//! that resumed the same session would each silently discard the other's
//! turns, tool records, cost and goal progress (last writer wins). A process
//! that owns a session therefore holds an advisory lock on
//! `<sessions>/<id>.lock` for as long as it keeps writing that session:
//!
//! - the lock is taken lazily by the first `save_session` and eagerly when a
//!   session is resumed (`--continue`, `--session`, `/sessions <id>`);
//! - a resume that finds the lock held by another live process continues in a
//!   forked copy under a new id instead of sharing the file;
//! - a save whose lock is held elsewhere is refused rather than clobbering.
//!
//! The lock uses the standard library's advisory file locks (`flock` on Unix,
//! `LockFileEx` on Windows), the same primitive the memory store uses. The
//! operating system releases it when the owning process exits or crashes, so a
//! lock file left behind is inert: it never blocks a later process. Lock files
//! are deliberately never deleted, because unlinking a lock file another
//! process has open would let two owners lock different inodes.

use std::collections::HashMap;
use std::fs::{File, TryLockError};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use compact_str::CompactString;
use uuid::Uuid;

use crate::session::Session;

/// Locks this process holds, keyed by lock-file path. Keeping the open file
/// alive keeps the lock; dropping it releases the lock.
static HELD: LazyLock<Mutex<HashMap<PathBuf, Arc<HeldSessionLock>>>> =
    LazyLock::new(Default::default);

#[derive(Debug, Default)]
struct LeaseState {
    revoked: bool,
    publishers: usize,
    native_frontier: Option<super::native_replay::FrontierIdentity>,
}

#[derive(Debug)]
struct HeldSessionLock {
    file: File,
    root: Arc<crate::paths::WorkspaceBinding>,
    identity: crate::fs::CheckedMetadata,
    name: std::ffi::OsString,
    io: Mutex<()>,
    state: Mutex<LeaseState>,
}

impl HeldSessionLock {
    fn validate_namespace(&self) -> anyhow::Result<()> {
        self.root.validate_private_root()?;
        let current = self
            .root
            .open_regular_relative(std::path::Path::new(&self.name))?;
        crate::fs::validate_private_file(&current)?;
        let path = self.root.root().join(&self.name);
        crate::fs::ensure_same_file(
            &path,
            &self.identity,
            &crate::fs::checked_file_metadata(&self.file)?,
        )?;
        crate::fs::ensure_same_file(
            &path,
            &self.identity,
            &crate::fs::checked_file_metadata(&current)?,
        )?;
        self.root.validate_private_root()?;
        Ok(())
    }

    fn revoke(&self, force_unlock: bool) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.revoked = true;
        // Forced exit must not unlock an in-flight native publisher while its
        // kernel operation can still replace the checkpoint. Process exit
        // terminates that writer. An inherited descriptor may delay native
        // recovery, which remains a truthful refusal rather than concurrent
        // ownership. Legacy force-stop unlocking stays unchanged.
        if state.publishers == 0 || (force_unlock && state.native_frontier.is_none()) {
            let _ = self.file.unlock();
        }
    }

    fn prepare_legacy_target(&self, session_id: &str) -> anyhow::Result<()> {
        use std::io::{Read, Seek};
        self.validate_namespace()?;
        let name = format!("{session_id}.json");
        let mut file = match self.root.open_regular_relative(std::path::Path::new(&name)) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "private file is not an owned regular file: {error}"
                ));
            }
        };
        // Classification is restrictive only: inspect the retained, owned
        // regular file without chmod. Native or uncertain bytes never enter
        // the legacy permission-repair path.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = file.metadata()?;
            anyhow::ensure!(
                metadata.is_file()
                    && metadata.uid() == crate::fs::private_current_uid()
                    && metadata.nlink() == 1,
                "private file is not an owned regular file"
            );
        }
        #[cfg(not(unix))]
        crate::fs::validate_private_file(&file)?;
        let identity = crate::fs::checked_file_metadata(&file)?;
        let mut bytes = Vec::new();
        file.by_ref()
            .take((super::storage::MAX_SESSION_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() <= super::storage::MAX_SESSION_BYTES,
            "saved session exceeds its finite limit"
        );
        #[derive(serde::Deserialize)]
        struct SavedClassification {
            #[serde(default)]
            native_frontier: super::native_replay::NativeFrontier,
            #[serde(default)]
            native_history: Option<serde::de::IgnoredAny>,
            #[serde(default)]
            retired_native_history: Vec<serde::de::IgnoredAny>,
        }
        let classification: SavedClassification = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("saved session compatibility cannot be classified"))?;
        anyhow::ensure!(
            classification.native_frontier.is_initial()
                && classification.native_history.is_none()
                && classification.retired_native_history.is_empty(),
            "unfinished native context requires its saved-session owner"
        );
        let current = self
            .root
            .open_regular_relative(std::path::Path::new(&name))?;
        crate::fs::ensure_same_file(
            &self.root.root().join(&name),
            &identity,
            &crate::fs::checked_file_metadata(&current)?,
        )?;
        file.rewind()?;
        let mut current_bytes = Vec::new();
        file.by_ref()
            .take((super::storage::MAX_SESSION_BYTES + 1) as u64)
            .read_to_end(&mut current_bytes)?;
        anyhow::ensure!(
            current_bytes == bytes,
            "saved session changed during classification"
        );
        self.validate_namespace()?;
        // Preserve the existing Unix legacy repair contract, on the exact
        // classified descriptor. No pathname is reopened for mutation.
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let metadata = file.metadata()?;
            anyhow::ensure!(
                metadata.is_file()
                    && metadata.uid() == crate::fs::private_current_uid()
                    && metadata.nlink() == 1,
                "private file is not an owned regular file"
            );
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        crate::fs::validate_private_file(&file)?;
        self.validate_namespace()?;
        Ok(())
    }

    fn publish(&self, session_id: &str, bytes: &[u8]) -> anyhow::Result<()> {
        crate::paths::validate_portable_component(session_id)?;
        anyhow::ensure!(
            self.name == std::ffi::OsStr::new(&format!("{session_id}.lock")),
            "checkpoint does not match its retained session lock"
        );
        self.validate_namespace()?;
        let parent = crate::fs::checked_file_metadata(&self.root.try_clone_directory_file()?)?;
        let name = format!("{session_id}.json");
        match self.root.open_regular_relative(std::path::Path::new(&name)) {
            Ok(existing) => crate::fs::validate_private_file(&existing)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        crate::fs::private_atomic_write_checked_sync(
            &self.root.root().join(&name),
            bytes,
            &parent,
        )?;
        self.validate_namespace()?;
        let published = self
            .root
            .open_regular_relative(std::path::Path::new(&name))?;
        crate::fs::validate_private_file(&published)?;
        self.root.validate_private_root()?;
        Ok(())
    }
}

/// Exact process-local writer lease; importing the same saved id does not
/// reconstruct it. Session switches revoke it before releasing ownership.
#[derive(Debug, Clone)]
pub(super) struct NativeWriterLease {
    held: Arc<HeldSessionLock>,
    frontier: super::native_replay::NativeFrontier,
    path: PathBuf,
}

impl NativeWriterLease {
    pub(super) fn matches(&self, session: &Session) -> bool {
        lock_path(&session.id).is_ok_and(|path| path == self.path)
            && self.frontier.identity().matches(&session.native_frontier)
    }

    pub(super) fn begin_write(&self) -> anyhow::Result<NativeWriterGuard> {
        self.held.validate_namespace()?;
        let mut state = self
            .held
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(
            !state.revoked,
            "unfinished native context no longer owns its saved session"
        );
        anyhow::ensure!(
            state.publishers < 64,
            "native context writer limit exceeded"
        );
        state.publishers += 1;
        Ok(NativeWriterGuard(Arc::clone(&self.held)))
    }

    pub(super) fn publish_checkpoint(&self, session: &Session, bytes: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.matches(session),
            "native checkpoint belongs to another session owner"
        );
        let _io = self
            .held
            .io
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.held.publish(&session.id, bytes)
    }
}

#[derive(Debug)]
pub(super) struct NativeWriterGuard(Arc<HeldSessionLock>);

impl NativeWriterGuard {
    pub(super) fn check(&self) -> anyhow::Result<()> {
        // Filesystem checks never hold the short revocation mutex; force stop
        // must remain able to revoke without waiting on a filesystem.
        self.0.validate_namespace()?;
        let state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(
            !state.revoked,
            "unfinished native context no longer owns its saved session"
        );
        Ok(())
    }
}

impl Drop for NativeWriterGuard {
    fn drop(&mut self) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.publishers -= 1;
        if state.revoked && state.publishers == 0 {
            let _ = self.0.file.unlock();
        }
    }
}

/// A strict host attachment, distinct from the legacy id-only claim. The
/// existing canonical owner must be the exact frontier being attached.
pub(super) fn claim_native_writer(session: &Session) -> anyhow::Result<NativeWriterLease> {
    anyhow::ensure!(
        !crate::paths::artifact_disabled("sessions"),
        "unfinished native context requires saved-session persistence"
    );
    anyhow::ensure!(
        claim_session(&session.id)? == SessionClaim::Owned,
        "unfinished native context is open in another process"
    );
    let path = lock_path(&session.id)?;
    let held = HELD.lock().unwrap_or_else(|error| error.into_inner());
    let lease = held
        .get(&path)
        .ok_or_else(|| anyhow::anyhow!("native session writer was released"))?;
    let mut state = lease
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    anyhow::ensure!(!state.revoked, "native session writer was released");
    if let Some(identity) = &state.native_frontier {
        anyhow::ensure!(
            identity.matches(&session.native_frontier),
            "unfinished native context belongs to another session owner"
        );
    } else {
        state.native_frontier = Some(session.native_frontier.identity());
    }
    Ok(NativeWriterLease {
        held: Arc::clone(lease),
        frontier: session.native_frontier.clone(),
        path,
    })
}

/// Restrictive classification only, never writer authority. An independently
/// decoded same-id snapshot cannot use legacy save while the canonical owner
/// has activated native publication, even before it has any opaque frames.
pub(super) fn requires_native_publication(session_id: &str) -> bool {
    let Ok(path) = lock_path(session_id) else {
        return false;
    };
    let held = HELD.lock().unwrap_or_else(|error| error.into_inner());
    held.get(&path).is_some_and(|lease| {
        lease
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .native_frontier
            .is_some()
    })
}

/// Legacy snapshots may not erase a native marker already stored under the
/// exact newly owned lock. No imported marker supplies a native permit.
pub(super) fn publish_legacy_owned(session: &Session, bytes: &[u8]) -> anyhow::Result<()> {
    let path = lock_path(&session.id)?;
    let held = HELD
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&path)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("saved-session writer was released"))?;
    // A synchronous legacy caller never waits on an active native writer's
    // mutex. Frontend native saves use their explicit owned async publisher.
    let _io = held
        .io
        .try_lock()
        .map_err(|_| anyhow::anyhow!("saved-session writer is busy"))?;
    held.validate_namespace()?;
    {
        let state = held.state.lock().unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(
            !state.revoked && state.native_frontier.is_none(),
            "native checkpoint requires its original owner"
        );
    }
    held.prepare_legacy_target(&session.id)?;
    held.publish(&session.id, bytes)
}

/// Result of trying to take ownership of a session id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionClaim {
    /// This process owns the session (now, or already did).
    Owned,
    /// Another live process holds the session's lock.
    HeldElsewhere,
}

/// The advisory lock file guarding one session's snapshot.
pub(crate) fn lock_path(session_id: &str) -> anyhow::Result<PathBuf> {
    crate::paths::validate_portable_component(session_id)?;
    Ok(crate::paths::process_paths()
        .expect("startup must initialize application paths")
        .sessions_dir()
        .join(format!("{session_id}.lock")))
}

fn open_lock_file(path: &std::path::Path) -> anyhow::Result<File> {
    if let Some(parent) = path.parent() {
        crate::paths::ensure_private_directory(parent)?;
    }
    // Create the empty lock file privately (0600 / protected DACL) the first
    // time; a concurrent creator racing us is equally fine.
    match crate::fs::private_atomic_create_sync(path, b"") {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    Ok(crate::fs::open_private_file(path)?)
}

/// Try to take ownership of `session_id` for the rest of this process's life
/// (or until [`release_session`]). Idempotent for a session this process
/// already owns.
pub fn claim_session(session_id: &str) -> anyhow::Result<SessionClaim> {
    if crate::paths::artifact_disabled("sessions") {
        return Ok(SessionClaim::Owned);
    }
    let path = lock_path(session_id)?;
    let cached = HELD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&path)
        .cloned();
    if let Some(lease) = cached {
        lease.validate_namespace()?;
        anyhow::ensure!(
            !lease
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .revoked,
            "saved session writer was released"
        );
        return Ok(SessionClaim::Owned);
    }
    let file = open_lock_file(&path)?;
    let root = Arc::new(crate::paths::WorkspaceBinding::capture(
        path.parent()
            .ok_or_else(|| anyhow::anyhow!("session lock has no parent"))?,
    )?);
    let lease = Arc::new(HeldSessionLock {
        identity: crate::fs::checked_file_metadata(&file)?,
        file,
        root,
        name: path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("session lock has no name"))?
            .to_os_string(),
        state: Mutex::default(),
        io: Mutex::default(),
    });
    lease.validate_namespace()?;
    match lease.file.try_lock() {
        Ok(()) => {
            tracing::debug!("session lock acquired: id={}", session_id);
            HELD.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(path, lease);
            Ok(SessionClaim::Owned)
        }
        Err(TryLockError::WouldBlock) => Ok(SessionClaim::HeldElsewhere),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

/// Give up ownership of `session_id` after switching to another session.
pub fn release_session(session_id: &str) {
    let Ok(path) = lock_path(session_id) else {
        return;
    };
    let mut held = HELD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(lease) = held.remove(&path) {
        // Unlock explicitly rather than relying on close: a child forked by a
        // concurrent spawn briefly shares this open file description until its
        // close-on-exec `exec`, and closing only our copy would leave the lock
        // held for that window.
        lease.revoke(false);
        tracing::debug!("session lock released: id={}", session_id);
    }
}

/// Give up every session this process owns. A force-stopped headless run
/// calls this before `exit` (mini-agent-p73n1): a stalled child may still
/// share a lock's open file description, which would otherwise keep the
/// session locked after this process is gone.
pub fn release_all() {
    let mut held = HELD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    for (_, lease) in held.drain() {
        lease.revoke(true);
    }
}

/// The error a save reports when another process owns the session.
pub(crate) fn held_elsewhere_error(session_id: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "session {session_id} is open in another mini-agent process; \
         refusing to overwrite its turns (resume it again to continue in a forked copy)"
    )
}

/// A fresh, independent copy of `source` under a new id. Persisted state is
/// copied through the same serialization a reload uses, so process-local
/// stores (todos, goal, read tracker) are not shared with the source.
pub fn fork_session(source: &Session) -> anyhow::Result<Session> {
    let mut fork =
        super::storage::decode_session_json(&super::storage::serialize_bounded_session(source)?)?;
    fork.native_frontier = source.native_frontier.fork();
    fork.id = CompactString::new(Uuid::new_v4().to_string());
    if !fork.name.is_empty() {
        fork.name = CompactString::new(format!("{} (fork)", fork.name));
    }
    fork.updated_at = CompactString::new(chrono::Utc::now().to_rfc3339());
    Ok(fork)
}

/// Explicit branching uses the rewind list's one-based user-turn order.
/// A selected turn keeps that input and its typed media, before its answer;
/// omitting it keeps the complete current history. No source state is mutated.
pub(crate) fn fork_at_turn(source: &Session, turn: Option<usize>) -> anyhow::Result<Session> {
    let length = match turn {
        None => source.messages.len(),
        Some(0) => anyhow::bail!("fork turn numbers start at 1"),
        Some(turn) => source
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message.role == super::MessageRole::User)
            .nth(turn - 1)
            .map(|(index, _)| index + 1)
            .ok_or_else(|| anyhow::anyhow!("fork turn is outside the conversation"))?,
    };
    let mut fork = fork_session(source)?;
    fork.truncate_to(length);
    fork.rewind_undo = None;
    fork.permission_allowlist.clear();
    if fork.name.is_empty() {
        fork.name = "session (fork)".into();
    }
    Ok(fork)
}

/// A resumed session this process may write, plus the notice to show when it
/// had to be forked.
pub struct ResumedSession {
    pub session: Session,
    pub notice: Option<String>,
}

/// Take ownership of a session selected for resumption. When another live
/// process already owns it, continue in a fork instead of clobbering that
/// process's turns. A lock that cannot be established at all (for example on
/// a filesystem without advisory locks) is reported and the resume proceeds
/// unguarded rather than failing.
pub fn claim_or_fork(session: Session) -> anyhow::Result<ResumedSession> {
    match claim_session(&session.id) {
        Ok(SessionClaim::Owned) => Ok(ResumedSession {
            session,
            notice: None,
        }),
        Ok(SessionClaim::HeldElsewhere) => {
            let fork = fork_session(&session)?;
            // A brand-new id cannot be held by anyone else; claiming it now
            // keeps a third process from resuming the fork concurrently.
            if let Err(error) = claim_session(&fork.id) {
                tracing::warn!("session lock for fork {} unavailable: {}", fork.id, error);
            }
            let notice = format!(
                "session {} is in use by another mini-agent process; continuing in a forked copy {} so neither process overwrites the other's turns",
                session.id, fork.id
            );
            Ok(ResumedSession {
                session: fork,
                notice: Some(notice),
            })
        }
        Err(error) => {
            tracing::warn!(
                "session lock for {} unavailable, resuming unguarded: {}",
                session.id,
                error
            );
            Ok(ResumedSession {
                session,
                notice: None,
            })
        }
    }
}

#[cfg(test)]
mod fork75_tests {
    use super::*;
    use crate::session::{MessageRole, PermissionAllowEntry};

    fn conversation() -> Session {
        let mut session = Session::new("openrouter", "fixture", 128_000, "branch");
        for (role, text) in [
            (MessageRole::User, "first"),
            (MessageRole::Assistant, "first answer"),
            (MessageRole::User, "second"),
            (MessageRole::Assistant, "second answer"),
            (MessageRole::User, "hidden redo"),
        ] {
            session.add_message(role, text);
        }
        session.rewind_to(4);
        session.total_input_tokens = 80;
        session.total_output_tokens = 20;
        session.total_cost = 0.5;
        session.permission_allowlist.push(PermissionAllowEntry {
            tool: "shell".into(),
            pattern: "*".into(),
        });
        session
    }

    #[test]
    fn fork75_branch_keeps_selected_input_costs_and_original_without_grants_or_redo() {
        let source = conversation();
        let before = serde_json::to_vec(&source).unwrap();
        let fork = fork_at_turn(&source, Some(2)).unwrap();
        assert_ne!(fork.id, source.id);
        assert_eq!(fork.name, "branch (fork)");
        assert_eq!(fork.messages.len(), 3);
        assert_eq!(fork.messages[2].content, "second");
        assert_eq!(fork.total_input_tokens, 80);
        assert_eq!(fork.total_output_tokens, 20);
        assert_eq!(fork.total_cost, 0.5);
        assert!(fork.permission_allowlist.is_empty());
        assert!(fork.rewind_undo.is_none());
        assert_eq!(serde_json::to_vec(&source).unwrap(), before);
        let full = fork_at_turn(&source, None).unwrap();
        assert_eq!(full.messages.len(), 4);
        assert_eq!(full.messages[3].content, "second answer");
    }

    #[test]
    fn fork75_selected_turn_preserves_ordered_media_exactly() {
        use crate::session::user::{ImageMime, PersistedUserMessage, UserBlock};
        let mut source = conversation();
        let user = PersistedUserMessage::new(vec![
            UserBlock::Text {
                text: "before image".into(),
            },
            UserBlock::Image {
                data: "aW5lcnQ=".into(),
                mime_type: ImageMime::Png,
            },
            UserBlock::Text {
                text: "after image".into(),
            },
        ])
        .unwrap();
        source.add_user_message(user.clone());
        source.add_message(MessageRole::Assistant, "later answer");
        let before = serde_json::to_vec(&source).unwrap();
        let fork = fork_at_turn(&source, Some(3)).unwrap();
        assert_eq!(fork.messages.len(), 5);
        assert_eq!(fork.messages[4].user.as_ref(), Some(&user));
        assert_eq!(fork.messages[4].content, "before image\nafter image");
        assert_eq!(serde_json::to_vec(&source).unwrap(), before);
    }

    #[test]
    fn fork75_invalid_turn_and_oversized_source_refuse_without_mutation() {
        let mut source = conversation();
        let before = serde_json::to_vec(&source).unwrap();
        for turn in [0, 3, usize::MAX] {
            assert!(fork_at_turn(&source, Some(turn)).is_err());
        }
        assert_eq!(serde_json::to_vec(&source).unwrap(), before);
        source.add_message(
            MessageRole::User,
            &"x".repeat(super::super::storage::MAX_SESSION_BYTES),
        );
        assert!(fork_at_turn(&source, None).is_err());
    }

    #[test]
    fn fork75_cli_requires_resume_and_rejects_ephemeral_fork() {
        use clap::Parser;
        for args in [
            vec!["mini-agent", "-c", "--fork-session"],
            vec!["mini-agent", "--session", "example", "--fork-session"],
            vec!["mini-agent", "--resume", "--fork-session"],
        ] {
            assert!(crate::cli::Cli::try_parse_from(args).unwrap().fork_session);
        }
        for args in [
            vec!["mini-agent", "--fork-session"],
            vec!["mini-agent", "-c", "--fork-session", "--no-session"],
        ] {
            assert!(crate::cli::Cli::try_parse_from(args).is_err());
        }
    }
}
