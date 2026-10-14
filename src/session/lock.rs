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
use std::sync::{LazyLock, Mutex};

use compact_str::CompactString;
use uuid::Uuid;

use crate::session::Session;

/// Locks this process holds, keyed by lock-file path. Keeping the open file
/// alive keeps the lock; dropping it releases the lock.
static HELD: LazyLock<Mutex<HashMap<PathBuf, File>>> = LazyLock::new(Default::default);

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
    let mut held = HELD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if held.contains_key(&path) {
        return Ok(SessionClaim::Owned);
    }
    let file = open_lock_file(&path)?;
    match file.try_lock() {
        Ok(()) => {
            tracing::debug!("session lock acquired: id={}", session_id);
            held.insert(path, file);
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
    if let Some(file) = held.remove(&path) {
        // Unlock explicitly rather than relying on close: a child forked by a
        // concurrent spawn briefly shares this open file description until its
        // close-on-exec `exec`, and closing only our copy would leave the lock
        // held for that window.
        let _ = file.unlock();
        tracing::debug!("session lock released: id={}", session_id);
    }
}

/// Give up every session this process owns. A force-stopped headless run
/// calls this before `exit` (mini-agent-p73n1): a stalled child may still
/// share a lock's open file description, which would otherwise keep the
/// session locked after this process is gone.
pub fn release_all() {
    let mut held = HELD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    for (_, file) in held.drain() {
        let _ = file.unlock();
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
