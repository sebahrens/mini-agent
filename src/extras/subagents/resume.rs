//! Resumable child conversations.
//!
//! After a child returns a report, the host keeps its conversation (prompt,
//! tool calls, tool results and final text) under a fresh child id and
//! prints that id after the child's quoted output. A later
//! `task {"resume": "<id>", "prompts": ["follow-up"]}` call continues that
//! conversation with the follow-up as a new user turn, through the same
//! permission check, persona, tools, cost budget and report contract as any
//! other call. The store is bounded and the oldest conversation is dropped
//! first. A session-owned store is also persisted next to the session's other
//! private artifacts (see [`session_store_path`]) so `--continue`/`--resume`
//! restores the ids that fit the limits again; a standalone store is memory only.

use std::collections::VecDeque;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rig::completion::Message;

/// Default conversations kept per session (or per task tool without a session
/// pool). `[subagents] task_resume_max_children` overrides it.
pub(crate) const MAX_RESUMABLE_CHILDREN: usize = 16;
/// Default serialized size above which a conversation is not kept for
/// resumption. `[subagents] task_resume_max_bytes` overrides it.
pub(crate) const MAX_RESUMABLE_BYTES: usize = 8 * 1024 * 1024;
/// Hard ceilings for the configured limits.
const MAX_RESUMABLE_CHILDREN_CEILING: usize = 256;
const MAX_RESUMABLE_BYTES_CEILING: usize = 256 * 1024 * 1024;

/// Effective resume-store limits (configured, clamped, or the defaults).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResumeLimits {
    pub(crate) children: usize,
    pub(crate) bytes: usize,
}

impl Default for ResumeLimits {
    fn default() -> Self {
        Self {
            children: MAX_RESUMABLE_CHILDREN,
            bytes: MAX_RESUMABLE_BYTES,
        }
    }
}

impl ResumeLimits {
    pub(crate) fn from_config(config: &crate::config::Config) -> Self {
        let defaults = Self::default();
        let Some(policy) = config.subagents.as_ref() else {
            return defaults;
        };
        Self {
            children: policy
                .task_resume_max_children
                .unwrap_or(defaults.children)
                .clamp(1, MAX_RESUMABLE_CHILDREN_CEILING),
            bytes: policy
                .task_resume_max_bytes
                .unwrap_or(defaults.bytes)
                .clamp(1024, MAX_RESUMABLE_BYTES_CEILING),
        }
    }
}

/// What [`ResumeStore::insert_with`] did with a finished conversation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Insert {
    /// Kept under `id`; `evicted` are the ids dropped to make room.
    Stored { id: String, evicted: Vec<String> },
    /// Nothing to resume (empty history).
    Empty,
    /// Not kept: the serialized conversation is larger than the byte limit.
    TooLarge { bytes: usize, limit: usize },
}

/// Host line telling the parent why a child cannot be resumed or what the
/// store dropped, so a missing or expired resume id is never a surprise.
pub(crate) fn insert_note(outcome: &Insert, limits: ResumeLimits) -> Option<String> {
    match outcome {
        Insert::Stored { evicted, .. } if !evicted.is_empty() => Some(format!(
            "[host note: the resume store keeps {} children ([subagents] task_resume_max_children); \
             dropped the oldest resumable child: {}]\n",
            limits.children,
            evicted.join(", ")
        )),
        Insert::TooLarge { bytes, limit } => Some(format!(
            "[host note: child not resumable: conversation exceeds {} ({} serialized); \
             raise [subagents] task_resume_max_bytes or start a new task]\n",
            human_mib(*limit),
            human_mib(*bytes)
        )),
        _ => None,
    }
}

fn human_mib(bytes: usize) -> String {
    if bytes >= 1024 * 1024 && bytes.is_multiple_of(1024 * 1024) {
        format!("{} MiB", bytes / (1024 * 1024))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KiB", bytes.div_ceil(1024))
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredChild {
    pub id: String,
    /// The `agent_type` the child ran as (`None`: the default explorer).
    pub agent_type: Option<String>,
    pub history: Vec<Message>,
}

#[derive(Debug, Default)]
pub(crate) struct ResumeStore {
    children: Mutex<VecDeque<StoredChild>>,
    /// Private file the store is mirrored to after every change.
    persist: Option<PathBuf>,
}

const PERSIST_VERSION: u32 = 1;
const MAX_AGENT_TYPE_BYTES: usize = 64;

#[derive(serde::Deserialize)]
struct PersistedFile {
    version: u32,
    children: Vec<StoredChild>,
}

#[derive(serde::Deserialize)]
struct PersistedIds {
    version: u32,
    children: Vec<PersistedId>,
}

#[derive(serde::Deserialize)]
struct PersistedId {
    id: String,
}

/// Where a session's resumable children are kept: a name derived only from the
/// session id inside that session's private tool-output directory, so it is
/// deleted, pruned and swept together with the session. `None` when session
/// artifacts are unavailable.
pub(crate) fn session_store_path(session_id: &str) -> Option<PathBuf> {
    let dir = crate::session::storage::command_log_dir(session_id)?;
    let name =
        crate::paths::digest_filename("subagent-resume-store", &[session_id.as_bytes()], "json")
            .ok()?;
    Some(dir.join(name))
}

/// Reads the persisted file with the hard ceilings; a missing, unsafe,
/// oversized or malformed file yields no children (and a warning when it
/// exists), never an error that blocks the session.
fn read_persisted(path: &Path) -> Option<Vec<u8>> {
    let file = match crate::fs::open_private_file(path) {
        Ok(file) => file,
        Err(error) => {
            if path.exists() {
                tracing::warn!(%error, path = %path.display(), "ignoring unreadable resumable-child store");
            }
            return None;
        }
    };
    let mut bytes = Vec::new();
    file.take(MAX_RESUMABLE_BYTES_CEILING as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_RESUMABLE_BYTES_CEILING {
        tracing::warn!(path = %path.display(), "ignoring oversized resumable-child store");
        return None;
    }
    Some(bytes)
}

fn valid_child_id(id: &str) -> bool {
    id.len() <= 64 && uuid::Uuid::parse_str(id).is_ok()
}

/// Ids a resumed session can still continue, read from its persisted store.
/// Used to limit the "no longer resumable" notice to the ids that were lost.
pub(crate) fn restorable_ids(session_id: &str) -> std::collections::HashSet<String> {
    let Some(bytes) = session_store_path(session_id).and_then(|path| read_persisted(&path)) else {
        return Default::default();
    };
    match serde_json::from_slice::<PersistedIds>(&bytes) {
        Ok(file) if file.version == PERSIST_VERSION => file
            .children
            .into_iter()
            .map(|child| child.id)
            .filter(|id| valid_child_id(id))
            .collect(),
        _ => Default::default(),
    }
}

impl ResumeStore {
    /// A store mirrored to `path`, preloaded with the children it holds.
    pub(crate) fn persistent(path: PathBuf) -> Self {
        let mut children = VecDeque::new();
        if let Some(bytes) = read_persisted(&path) {
            match serde_json::from_slice::<PersistedFile>(&bytes) {
                Ok(file)
                    if file.version == PERSIST_VERSION
                        && file.children.len() <= MAX_RESUMABLE_CHILDREN_CEILING =>
                {
                    for child in file.children {
                        let valid = valid_child_id(&child.id)
                            && !child.history.is_empty()
                            && child
                                .agent_type
                                .as_deref()
                                .is_none_or(|name| name.len() <= MAX_AGENT_TYPE_BYTES)
                            && !children
                                .iter()
                                .any(|kept: &StoredChild| kept.id == child.id);
                        if valid {
                            children.push_back(child);
                        }
                    }
                }
                _ => tracing::warn!(
                    path = %path.display(),
                    "ignoring malformed resumable-child store"
                ),
            }
        }
        Self {
            children: Mutex::new(children),
            persist: Some(path),
        }
    }

    /// Mirrors the store to its file: the newest children whose serialized
    /// size sums to at most `limits.bytes` (the newest is always kept). An
    /// empty store removes the file. Failures are logged; resumption in this
    /// process is unaffected.
    fn persist_locked(&self, children: &VecDeque<StoredChild>, limits: ResumeLimits) {
        let Some(path) = &self.persist else {
            return;
        };
        let mut parts: Vec<Vec<u8>> = Vec::new();
        let mut total = 0usize;
        for child in children.iter().rev() {
            let Ok(bytes) = serde_json::to_vec(child) else {
                continue;
            };
            if !parts.is_empty() && total.saturating_add(bytes.len()) > limits.bytes {
                break;
            }
            total += bytes.len();
            parts.push(bytes);
        }
        if parts.is_empty() {
            let _ = std::fs::remove_file(path);
            return;
        }
        parts.reverse();
        let mut out = format!("{{\"version\":{PERSIST_VERSION},\"children\":[").into_bytes();
        for (index, part) in parts.iter().enumerate() {
            if index > 0 {
                out.push(b',');
            }
            out.extend_from_slice(part);
        }
        out.extend_from_slice(b"]}");
        let result = path
            .parent()
            .map_or(Ok(()), |dir| crate::paths::ensure_private_directory(dir))
            .map_err(anyhow::Error::from)
            .and_then(|()| {
                crate::fs::private_atomic_write_sync(path, &out).map_err(anyhow::Error::from)
            });
        if let Err(error) = result {
            tracing::warn!(%error, "failed to persist resumable children");
        }
    }

    /// Keep `history` for resumption under the default limits and return its
    /// id, or `None` when it is empty or too large.
    #[cfg(test)]
    pub(crate) fn insert(
        &self,
        agent_type: Option<String>,
        history: Vec<Message>,
    ) -> Option<String> {
        match self.insert_with(ResumeLimits::default(), agent_type, history) {
            Insert::Stored { id, .. } => Some(id),
            _ => None,
        }
    }

    pub(crate) fn insert_with(
        &self,
        limits: ResumeLimits,
        agent_type: Option<String>,
        history: Vec<Message>,
    ) -> Insert {
        if history.is_empty() {
            return Insert::Empty;
        }
        // An unserializable history is reported as over the limit rather than
        // silently kept or dropped.
        let bytes = serde_json::to_vec(&history).map_or(usize::MAX, |bytes| bytes.len());
        if bytes > limits.bytes {
            return Insert::TooLarge {
                bytes,
                limit: limits.bytes,
            };
        }
        let id = uuid::Uuid::new_v4().to_string();
        let mut children = self.children.lock().unwrap_or_else(|e| e.into_inner());
        children.push_back(StoredChild {
            id: id.clone(),
            agent_type,
            history,
        });
        let mut evicted = Vec::new();
        while children.len() > limits.children {
            if let Some(dropped) = children.pop_front() {
                evicted.push(dropped.id);
            }
        }
        self.persist_locked(&children, limits);
        Insert::Stored { id, evicted }
    }

    pub(crate) fn get(&self, id: &str) -> Option<StoredChild> {
        self.children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|child| child.id == id)
            .cloned()
    }
}

/// The host line printed after a resumable child's quoted output.
pub(crate) fn resume_line(id: &str) -> String {
    format!(
        "[child id: {id}; continue this child's conversation with task {{\"resume\": \"{id}\", \"prompts\": [\"<follow-up>\"]}}]\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_store_is_bounded_and_drops_the_oldest_conversation() {
        let store = ResumeStore::default();
        assert!(
            store.insert(None, Vec::new()).is_none(),
            "nothing to resume"
        );
        let ids = (0..=MAX_RESUMABLE_CHILDREN)
            .map(|index| {
                store
                    .insert(
                        Some("explore".into()),
                        vec![Message::user(format!("q{index}"))],
                    )
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(store.get(&ids[0]).is_none(), "oldest dropped");
        let last = store.get(ids.last().unwrap()).unwrap();
        assert_eq!(last.agent_type.as_deref(), Some("explore"));
        assert_eq!(last.history.len(), 1);
        let huge = vec![Message::user("x".repeat(MAX_RESUMABLE_BYTES))];
        assert!(store.insert(None, huge).is_none(), "oversized conversation");
    }

    #[test]
    fn oversize_and_eviction_are_reported_and_limits_are_configurable() {
        let store = ResumeStore::default();
        let small = ResumeLimits {
            children: 2,
            bytes: 2048,
        };
        let big = vec![Message::user("x".repeat(4096))];
        let outcome = store.insert_with(small, None, big);
        let Insert::TooLarge { bytes, limit } = outcome else {
            panic!("expected TooLarge, got {outcome:?}");
        };
        assert!(bytes > 4096 && limit == 2048);
        let note = insert_note(&outcome, small).unwrap();
        assert!(
            note.contains("child not resumable: conversation exceeds 2 KiB"),
            "{note}"
        );
        assert!(note.contains("task_resume_max_bytes"), "{note}");
        let mut ids = Vec::new();
        let mut last = None;
        for index in 0..3 {
            let outcome = store.insert_with(small, None, vec![Message::user(format!("q{index}"))]);
            let Insert::Stored { id, evicted } = &outcome else {
                panic!("expected Stored");
            };
            ids.push(id.clone());
            last = Some((insert_note(&outcome, small), evicted.clone()));
        }
        let (note, evicted) = last.unwrap();
        assert_eq!(evicted, vec![ids[0].clone()]);
        assert!(note.unwrap().contains(&ids[0]));
        assert!(store.get(&ids[0]).is_none() && store.get(&ids[2]).is_some());
        assert_eq!(
            insert_note(&store.insert_with(small, None, Vec::new()), small),
            None
        );
    }

    fn store_file(name: &str) -> (crate::tests::OwnedTestDirectory, PathBuf) {
        let dir = crate::tests::OwnedTestDirectory::new(name);
        let path = dir.path().join("store").join("resume.json");
        (dir, path)
    }

    #[test]
    fn persisted_children_are_restored_by_a_new_store_and_stay_bounded() {
        let (_dir, path) = store_file("resume-persist-roundtrip");
        let limits = ResumeLimits {
            children: 2,
            bytes: 4096,
        };
        let first = ResumeStore::persistent(path.clone());
        let mut ids = Vec::new();
        for index in 0..3 {
            let outcome = first.insert_with(
                limits,
                Some("review".into()),
                vec![Message::user(format!("q{index}"))],
            );
            let Insert::Stored { id, .. } = outcome else {
                panic!("stored");
            };
            ids.push(id);
        }
        // A restart: a new store over the same file restores the kept children.
        let second = ResumeStore::persistent(path.clone());
        assert!(
            second.get(&ids[0]).is_none(),
            "evicted child is not persisted"
        );
        let restored = second.get(&ids[2]).expect("newest child restored");
        assert_eq!(restored.agent_type.as_deref(), Some("review"));
        assert_eq!(restored.history.len(), 1);
        assert!(second.get(&ids[1]).is_some());
        // The file never holds more serialized bytes than the limit (newest kept).
        let tight = ResumeLimits {
            children: 8,
            bytes: 300,
        };
        let big = vec![Message::user("y".repeat(150))];
        let third = ResumeStore::persistent(path.clone());
        let Insert::Stored { id: newest, .. } = third.insert_with(tight, None, big) else {
            panic!("stored");
        };
        let after = ResumeStore::persistent(path.clone());
        assert!(after.get(&newest).is_some());
        assert!(
            after.get(&ids[2]).is_none(),
            "older children trimmed to the byte limit"
        );
        assert_eq!(restorable_ids_at(&path), vec![newest]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode & 0o077, 0, "private file mode, got {mode:o}");
        }
    }

    fn restorable_ids_at(path: &Path) -> Vec<String> {
        let bytes = read_persisted(path).unwrap();
        serde_json::from_slice::<PersistedIds>(&bytes)
            .unwrap()
            .children
            .into_iter()
            .map(|child| child.id)
            .collect()
    }

    #[test]
    fn malformed_or_hostile_persisted_stores_restore_nothing_unsafe() {
        let (_dir, path) = store_file("resume-persist-hostile");
        let write = |text: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            crate::paths::ensure_private_directory(path.parent().unwrap()).unwrap();
            crate::fs::private_atomic_write_sync(&path, text.as_bytes()).unwrap();
        };
        let good = uuid::Uuid::new_v4().to_string();
        let message = serde_json::to_string(&Message::user("hi")).unwrap();
        for hostile in [
            "not json".to_string(),
            r#"{"version":9,"children":[]}"#.to_string(),
            format!(
                r#"{{"version":1,"children":[{{"id":"../../etc","agent_type":null,"history":[{message}]}}]}}"#
            ),
            format!(
                r#"{{"version":1,"children":[{{"id":"{good}","agent_type":"{}","history":[{message}]}}]}}"#,
                "a".repeat(200)
            ),
            format!(
                r#"{{"version":1,"children":[{{"id":"{good}","agent_type":null,"history":[]}}]}}"#
            ),
        ] {
            write(&hostile);
            let store = ResumeStore::persistent(path.clone());
            assert!(store.get(&good).is_none(), "{hostile}");
            assert!(store.children.lock().unwrap().is_empty(), "{hostile}");
        }
        // Duplicate ids collapse to one child.
        write(&format!(
            r#"{{"version":1,"children":[{{"id":"{good}","agent_type":null,"history":[{message}]}},{{"id":"{good}","agent_type":null,"history":[{message}]}}]}}"#
        ));
        assert_eq!(
            ResumeStore::persistent(path.clone())
                .children
                .lock()
                .unwrap()
                .len(),
            1
        );
        // A symlinked store is refused instead of followed.
        #[cfg(unix)]
        {
            let target = path.parent().unwrap().join("elsewhere.json");
            std::fs::write(&target, "{}").unwrap();
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(&target, &path).unwrap();
            assert!(read_persisted(&path).is_none());
        }
    }

    #[test]
    fn limits_come_from_config_with_ceilings() {
        let mut config = crate::config::Config::default();
        assert_eq!(ResumeLimits::from_config(&config), ResumeLimits::default());
        config.subagents = Some(crate::config::types::SubagentPolicy {
            task_resume_max_children: Some(0),
            task_resume_max_bytes: Some(usize::MAX),
            ..Default::default()
        });
        let limits = ResumeLimits::from_config(&config);
        assert_eq!(limits.children, 1);
        assert_eq!(limits.bytes, MAX_RESUMABLE_BYTES_CEILING);
        config.subagents = Some(crate::config::types::SubagentPolicy {
            task_resume_max_children: Some(40),
            task_resume_max_bytes: Some(1 << 20),
            ..Default::default()
        });
        assert_eq!(
            ResumeLimits::from_config(&config),
            ResumeLimits {
                children: 40,
                bytes: 1 << 20
            }
        );
    }
}
