//! A cheap append-only journal of the turn in progress.
//!
//! For snapshot sessions the whole session file is saved only when a turn
//! settles (re-serialising it per tool event is what `mini-agent-ldyyq`
//! removed). A crash, SIGKILL or power loss in the middle of a long agentic
//! turn would therefore lose every tool call and result of that turn although
//! the edits are on disk. While a turn is pending this module appends one
//! JSONL record per new transcript message, plus the file-checkpoint records
//! the turn created, to `<session id>.turn.jsonl`; the next load replays the
//! journal into the session and closes the turn with the usual "the previous
//! turn was cut off" host instruction. The journal is deleted at the next
//! settled save.
//!
//! The journal is valid only for the snapshot it was started against: its
//! header records the snapshot's message count, and a snapshot with another
//! count (the turn did settle) makes the journal stale.
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::session::{MessageRole, Session, SessionMessage};

/// `fsync` is batched: at most once per interval, so a kill loses at most the
/// last moments rather than costing a sync per event.
const SYNC_INTERVAL: Duration = Duration::from_millis(750);
const VERSION: u32 = 1;

struct State {
    /// Messages of the session already journaled (a prefix length).
    written: usize,
    /// File-checkpoint revision last journaled.
    revision: u64,
    bytes: usize,
    last_sync: Instant,
    /// The journal could not be kept consistent (compaction rewrote the
    /// history, size limit, I/O error); nothing more is written this turn.
    poisoned: bool,
    file: Option<std::fs::File>,
}

static STATES: Mutex<Option<HashMap<String, State>>> = Mutex::new(None);

fn with_states<T>(f: impl FnOnce(&mut HashMap<String, State>) -> T) -> T {
    let mut guard = STATES.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(HashMap::new))
}

fn open_new(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

/// Journal the messages and checkpoint records the pending turn added since
/// the last call. `base_len` is the session's message count before the turn's
/// user message (the count of the on-disk snapshot). Best effort: a failure
/// disables the journal for the turn and never fails the turn.
pub(crate) fn append(session: &Session, base_len: usize) {
    if crate::paths::artifact_disabled("sessions") {
        return;
    }
    let id = session.id.to_string();
    with_states(|states| {
        let result = append_locked(states, session, base_len);
        if let Err(error) = result {
            tracing::warn!(%error, "turn journal disabled for this turn");
            if let Some(state) = states.get_mut(&id) {
                state.poisoned = true;
                state.file = None;
            }
            if let Ok(path) = crate::session::storage::turn_journal_path(&id) {
                let _ = std::fs::remove_file(path);
            }
        }
    });
}

fn append_locked(
    states: &mut HashMap<String, State>,
    session: &Session,
    base_len: usize,
) -> anyhow::Result<()> {
    let id = session.id.to_string();
    if !states.contains_key(&id) {
        let path = crate::session::storage::turn_journal_path(&id)?;
        let mut file = open_new(&path)?;
        let header = serde_json::json!({
            "kind": "header",
            "version": VERSION,
            "session": id,
            "base_len": base_len,
        });
        let line = format!("{header}\n");
        file.write_all(line.as_bytes())?;
        states.insert(
            id.clone(),
            State {
                written: base_len,
                revision: u64::MAX,
                bytes: line.len(),
                last_sync: Instant::now(),
                poisoned: false,
                file: Some(file),
            },
        );
    }
    let state = states.get_mut(&id).expect("journal state exists");
    if state.poisoned {
        return Ok(());
    }
    anyhow::ensure!(
        session.messages.len() >= state.written,
        "the transcript was rewritten during the turn"
    );
    let mut lines = String::new();
    for message in &session.messages[state.written..] {
        let record = serde_json::json!({"kind": "message", "message": message});
        lines.push_str(&record.to_string());
        lines.push('\n');
    }
    let revision = session.file_checkpoints.revision();
    if revision != state.revision {
        let delta = session.file_checkpoints.journal_delta(base_len);
        lines.push_str(&serde_json::json!({"kind": "checkpoints", "delta": delta}).to_string());
        lines.push('\n');
    }
    if lines.is_empty() {
        return Ok(());
    }
    anyhow::ensure!(
        state.bytes + lines.len() <= crate::session::storage::MAX_SESSION_BYTES,
        "the turn journal reached the session size limit"
    );
    let file = state
        .file
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("journal file closed"))?;
    file.write_all(lines.as_bytes())?;
    state.bytes += lines.len();
    state.written = session.messages.len();
    state.revision = revision;
    if state.last_sync.elapsed() >= SYNC_INTERVAL {
        file.sync_data()?;
        state.last_sync = Instant::now();
    }
    Ok(())
}

/// Forget and delete the journal of `session_id`: the turn settled and the
/// snapshot holds everything.
pub(crate) fn discard(session_id: &str) {
    let known = with_states(|states| states.remove(session_id).is_some());
    // A journal left by an earlier process is stale once a snapshot settles.
    if let Ok(path) = crate::session::storage::turn_journal_path(session_id) {
        if known {
            let _ = std::fs::remove_file(path);
        } else if std::fs::symlink_metadata(&path).is_ok() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Replay a journal newer than the loaded snapshot into `session`: its
/// messages and checkpoint records, any call without a result closed as
/// outcome-unknown, and the "cut off" host instruction added. Returns whether
/// anything was recovered. The journal stays on disk (replay is repeatable)
/// until the next settled save deletes it.
pub(crate) fn recover(session: &mut Session) -> bool {
    let Ok(path) = crate::session::storage::turn_journal_path(&session.id) else {
        return false;
    };
    let Ok(file) = crate::fs::open_private_file(&path) else {
        return false;
    };
    let mut body = Vec::new();
    if file
        .take(crate::session::storage::MAX_SESSION_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .is_err()
        || body.len() > crate::session::storage::MAX_SESSION_BYTES
    {
        return false;
    }
    let text = String::from_utf8_lossy(&body);
    let mut lines = text.lines();
    let Some(header) = lines
        .next()
        .and_then(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    else {
        return false;
    };
    let valid = header["kind"] == "header"
        && header["version"] == VERSION
        && header["session"] == session.id.as_str()
        && header["base_len"].as_u64() == Some(session.messages.len() as u64);
    if !valid {
        return false;
    }
    let base_len = session.messages.len();
    let mut checkpoints = None;
    for line in lines {
        // A torn final line (killed mid-write) ends the replay.
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            break;
        };
        match record["kind"].as_str() {
            Some("message") => {
                let Ok(message) =
                    serde_json::from_value::<SessionMessage>(record["message"].clone())
                else {
                    break;
                };
                if session.messages.len() >= crate::session::storage::MAX_SESSION_MESSAGES {
                    break;
                }
                session.messages.push(message);
            }
            Some("checkpoints") => checkpoints = Some(record["delta"].clone()),
            _ => break,
        }
    }
    if session.messages.len() == base_len {
        return false;
    }
    if let Some(delta) = checkpoints
        && let Err(error) = session.file_checkpoints.apply_journal_delta(&delta)
    {
        tracing::warn!(%error, "journaled file checkpoints could not be restored");
    }
    close_unresolved_calls(session, base_len);
    crate::ui::record_interrupted_turn_notice(session);
    true
}

/// Give every tool call of the recovered turn that has no result an
/// outcome-unknown result, so the transcript stays protocol-complete.
fn close_unresolved_calls(session: &mut Session, turn_start: usize) {
    let resolved: std::collections::HashSet<String> = session.messages[turn_start..]
        .iter()
        .filter(|m| m.role == MessageRole::ToolResult)
        .filter_map(|m| m.tool_call_id.as_deref().map(str::to_owned))
        .collect();
    let mut unresolved: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for message in session.messages[turn_start..]
        .iter()
        .filter(|m| m.role == MessageRole::ToolCall)
    {
        let Some(id) = message.tool_call_id.as_deref() else {
            continue;
        };
        if !resolved.contains(id) && seen.insert(id.to_owned()) {
            let name = match &message.tool {
                Some(crate::session::PersistedToolMessage::Call { name, .. }) => name.to_string(),
                _ => "unknown".to_owned(),
            };
            unresolved.push((id.to_owned(), name));
        }
    }
    for (id, name) in unresolved {
        session.add_tool_result_with_id(&id, &name, crate::agent::runner::UNKNOWN_TOOL_OUTCOME);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn isolated(name: &str) -> crate::tests::OwnedTestDirectory {
        crate::tests::OwnedTestDirectory::new(name)
    }

    fn env(root: &crate::tests::OwnedTestDirectory) -> crate::tests::ScopedProcessEnv {
        crate::tests::ScopedProcessEnv::set(&[
            (
                "MINI_AGENT_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "MINI_AGENT_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "MINI_AGENT_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "MINI_AGENT_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
        ])
    }

    #[test]
    fn an_unsettled_turn_is_recovered_from_the_journal_after_a_kill() {
        let root = isolated("turn-journal");
        let _environment = env(&root);
        let mut session = Session::new("openrouter", "m", 128_000, "");
        session.add_message(MessageRole::User, "earlier prompt");
        session.add_message(MessageRole::Assistant, "earlier answer");
        // The settled snapshot, as saved before the turn started.
        crate::session::storage::save_session(&session).unwrap();
        let base_len = session.messages.len();

        // The turn: a prompt and two tool calls, one finished and one running.
        session.add_message(MessageRole::User, "do the work");
        session.add_tool_call_with_id("call-1", "bash", &serde_json::json!({"command": "ls"}));
        append(&session, base_len);
        session.add_tool_result_with_id("call-1", "bash", "file.txt");
        session.add_tool_call_with_id("call-2", "edit", &serde_json::json!({"path": "a"}));
        append(&session, base_len);
        // SIGKILL: the App is dropped without settling; reload from disk.
        with_states(|states| states.clear());
        let reloaded = crate::session::storage::load_session_exact(&session.id)
            .unwrap()
            .expect("the snapshot loads");
        assert_eq!(reloaded.messages.len(), base_len + 5 + 1 /* notice */);
        let roles: Vec<_> = reloaded.messages.iter().map(|m| m.role.clone()).collect();
        assert!(roles.contains(&MessageRole::ToolCall));
        let unresolved = reloaded
            .messages
            .iter()
            .find(|m| {
                m.role == MessageRole::ToolResult && m.tool_call_id.as_deref() == Some("call-2")
            })
            .expect("the running call is closed");
        assert!(unresolved.content.contains("outcome is unknown"));
        assert!(
            reloaded
                .messages
                .last()
                .unwrap()
                .content
                .contains("the previous turn was cut off")
        );

        // A settled save removes the journal; a snapshot of another length
        // never replays it.
        discard(&session.id);
        let path = crate::session::storage::turn_journal_path(&session.id).unwrap();
        assert!(!path.exists());
        let again = crate::session::storage::load_session_exact(&session.id)
            .unwrap()
            .unwrap();
        assert_eq!(again.messages.len(), base_len);
    }

    #[test]
    fn a_journal_for_another_snapshot_length_is_ignored() {
        let root = isolated("turn-journal-stale");
        let _environment = env(&root);
        let mut session = Session::new("openrouter", "m", 128_000, "");
        session.add_message(MessageRole::User, "first");
        crate::session::storage::save_session(&session).unwrap();
        session.add_message(MessageRole::Assistant, "settled later");
        // The journal was started against a one-message snapshot, but the turn
        // settled and the snapshot now holds two.
        append(&session, 1);
        with_states(|states| states.clear());
        crate::session::storage::save_session(&session).unwrap();
        let loaded = crate::session::storage::load_session_exact(&session.id)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.messages.len(), 2, "stale journal not replayed");
    }
}
