use std::io::Read;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::session::Session;

pub(crate) const MAX_SESSION_BYTES: usize = 8 * 1024 * 1024;
const MAX_SESSION_MESSAGES: usize = 100_000;

/// Decode only bounded data. Provider/workspace compatibility and fresh runtime
/// grants belong to the host's import controller, never saved labels.
pub(crate) fn decode_session_json(json: &str) -> anyhow::Result<Session> {
    anyhow::ensure!(
        json.len() <= MAX_SESSION_BYTES,
        "persisted session exceeds the 8 MiB limit"
    );
    let session: Session = serde_json::from_str(json)
        .map_err(|_| anyhow::anyhow!("invalid persisted session JSON"))?;
    anyhow::ensure!(
        session.messages.len() <= MAX_SESSION_MESSAGES,
        "persisted session exceeds the message limit"
    );
    Ok(session)
}

fn serialize_bounded_session(session: &Session) -> anyhow::Result<String> {
    session.validate_user_messages()?;
    anyhow::ensure!(
        session.messages.len() <= MAX_SESSION_MESSAGES,
        "persisted session exceeds the message limit"
    );
    struct BoundedJson(Vec<u8>);
    impl std::io::Write for BoundedJson {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .len()
                .checked_add(bytes.len())
                .filter(|size| *size <= MAX_SESSION_BYTES)
                .ok_or_else(|| {
                    std::io::Error::other("persisted session exceeds the 8 MiB limit")
                })?;
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    // Encode shared task/account owners once. A separate count pass would not
    // constrain a second snapshot if one of those owners changed meanwhile.
    let mut json = BoundedJson(Vec::new());
    serde_json::to_writer(&mut json, session)?;
    String::from_utf8(json.0).map_err(|_| anyhow::anyhow!("invalid persisted session UTF-8"))
}

/// Exact, noncreating and nonrepairing import. Both retained directories are
/// supplied by the host; stored working_dir is compared as text, never opened.
#[cfg(any(feature = "acp", test))]
#[allow(
    dead_code,
    reason = "shared API is intentionally dormant before its frontend consumer lands"
)]
pub(crate) fn load_session_exact_read_only(
    sessions_root: &crate::paths::WorkspaceBinding,
    requested_id: &str,
    workspace: &crate::paths::WorkspaceBinding,
) -> anyhow::Result<Option<Session>> {
    crate::paths::validate_portable_component(requested_id)?;
    sessions_root.validate_private_root()?;
    workspace.validate().map_err(anyhow::Error::msg)?;
    let relative = PathBuf::from(format!("{requested_id}.json"));
    let mut file = match sessions_root.open_regular_relative(&relative) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    crate::fs::validate_private_file(&file)?;
    let identity = crate::fs::checked_file_metadata(&file)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take((MAX_SESSION_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= MAX_SESSION_BYTES,
        "persisted session exceeds the 8 MiB limit"
    );
    crate::fs::validate_private_file(&file)?;
    let current = sessions_root.open_regular_relative(&relative)?;
    crate::fs::validate_private_file(&current)?;
    crate::fs::ensure_same_file(
        &sessions_root.logical_relative_path(&relative)?,
        &identity,
        &crate::fs::checked_file_metadata(&current)?,
    )?;
    sessions_root.validate_private_root()?;
    workspace.validate().map_err(anyhow::Error::msg)?;
    let json = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid persisted session UTF-8"))?;
    let mut session = decode_session_json(json)?;
    anyhow::ensure!(
        session.id == requested_id,
        "persisted session identity mismatch"
    );
    anyhow::ensure!(
        super::catalogue::same_workspace(&session.working_dir, workspace),
        "persisted session belongs to another workspace"
    );
    // Import history/task data, never explicit grants or concealed redo.
    session.permission_allowlist.clear();
    session.rewind_undo = None;
    Ok(Some(session))
}

fn session_dir() -> PathBuf {
    app_paths().sessions_dir()
}

pub(crate) fn session_path(session_id: &str) -> anyhow::Result<PathBuf> {
    crate::paths::validate_portable_component(session_id)?;
    Ok(session_dir().join(format!("{session_id}.json")))
}

pub fn tool_output_dir(session_id: &str) -> PathBuf {
    app_paths()
        .tool_outputs_dir()
        .join(crate::paths::opaque_name(
            "session-tool-output-directory",
            &[session_id.as_bytes()],
        ))
}

fn app_paths() -> crate::paths::AppPaths {
    crate::paths::process_paths().expect("startup must initialize application paths")
}

fn disabled(artifact: &'static str) -> bool {
    crate::paths::artifact_disabled(artifact)
}

pub(super) fn existing_session_dir() -> anyhow::Result<Option<PathBuf>> {
    let dir = session_dir();
    match std::fs::symlink_metadata(&dir) {
        Ok(_) => {
            crate::paths::ensure_private_directory(&dir)?;
            Ok(Some(dir))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_private_string(path: &Path) -> anyhow::Result<String> {
    let mut file = crate::fs::open_private_file(path).map_err(|error| {
        anyhow::anyhow!("refusing unsafe session file {}: {}", path.display(), error)
    })?;
    let mut content = String::new();
    file.by_ref()
        .take((MAX_SESSION_BYTES + 1) as u64)
        .read_to_string(&mut content)?;
    anyhow::ensure!(
        content.len() <= MAX_SESSION_BYTES,
        "persisted session exceeds the 8 MiB limit"
    );
    Ok(content)
}

#[cfg(test)]
pub(crate) fn config_path() -> PathBuf {
    app_paths().config_dir
}

/// Write `content` privately and atomically via a same-directory temporary
/// file. A crash mid-write leaves the previous version intact, and once this
/// returns `Ok` the new version is durable: the temporary file is `fsync`ed
/// before the rename and the directory after it (mini-agent-p73n1), so a
/// power loss cannot leave an undecodable session for `--continue` to skip.
pub fn atomic_write(path: &std::path::Path, content: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        crate::paths::ensure_private_directory(parent)?;
    }
    crate::fs::private_atomic_write_sync(path, content.as_bytes())?;
    Ok(())
}

#[cfg(all(test, unix))]
pub(crate) fn atomic_write_with_failure(
    path: &Path,
    content: &str,
    fail_rename: bool,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        crate::paths::ensure_private_directory(parent)?;
    }
    crate::fs::private_atomic_write_with_failure_sync(path, content.as_bytes(), fail_rename)?;
    Ok(())
}

pub fn save_session(session: &Session) -> anyhow::Result<()> {
    if disabled("sessions") {
        return Ok(());
    }
    let json = serialize_bounded_session(session)?;
    let dir = session_dir();
    crate::paths::ensure_private_directory(&dir)?;
    let path = session_path(&session.id)?;
    // The snapshot replaces the whole file, so only the process that owns the
    // session may write it; another owner's turns would otherwise be lost
    // (mini-agent-e42za). A lock that cannot be established at all does not
    // block the save: losing this process's own turns would be worse.
    match crate::session::lock::claim_session(&session.id) {
        Ok(crate::session::lock::SessionClaim::Owned) => {}
        Ok(crate::session::lock::SessionClaim::HeldElsewhere) => {
            return Err(crate::session::lock::held_elsewhere_error(&session.id));
        }
        Err(error) => {
            tracing::warn!("session lock for {} unavailable: {}", session.id, error);
        }
    }
    let json_len = json.len();
    atomic_write(&path, &json)?;
    tracing::debug!(
        "session saved: id={}, msgs={}, size={}",
        session.id,
        session.messages.len(),
        json_len,
    );
    Ok(())
}

pub fn save_tool_output(
    session_id: &str,
    tool_name: &str,
    output: &str,
) -> anyhow::Result<PathBuf> {
    if disabled("tool outputs") {
        anyhow::bail!("tool-output persistence is disabled by a legacy-path conflict");
    }
    let dir = tool_output_dir(session_id);
    crate::paths::ensure_private_directory(&dir)?;
    let nonce = Uuid::new_v4().to_string();
    let filename = crate::paths::digest_filename(
        "session-tool-output",
        &[
            session_id.as_bytes(),
            tool_name.as_bytes(),
            nonce.as_bytes(),
        ],
        "txt",
    )?;
    let path = dir.join(filename);
    crate::fs::private_atomic_create_sync(&path, output.as_bytes())?;
    Ok(path)
}

/// Remove a tool-output artifact created by an uncommitted UI turn. The path
/// is accepted only when it is an immediate child of this session's private,
/// opaque output directory; callers cannot use rollback bookkeeping as an
/// arbitrary-file deletion primitive.
pub(crate) fn delete_uncommitted_tool_output(session_id: &str, path: &Path) -> anyhow::Result<()> {
    let expected_dir = tool_output_dir(session_id);
    if path.parent() != Some(expected_dir.as_path()) {
        anyhow::bail!("refusing tool-output rollback outside the session directory");
    }
    crate::paths::ensure_private_directory(&expected_dir)?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                anyhow::bail!("refusing non-regular tool-output rollback target");
            }
            drop(crate::fs::open_private_file(path)?);
            std::fs::remove_file(path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub fn delete_session(id: &str) -> anyhow::Result<()> {
    if disabled("sessions") {
        return Ok(());
    }
    crate::paths::validate_portable_component(id)?;
    let Some(dir) = existing_session_dir()? else {
        tracing::debug!("session delete skipped (not found): id={}", id);
        return Ok(());
    };
    let path = dir.join(format!("{}.json", id));
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            drop(crate::fs::open_private_file(&path)?);
            std::fs::remove_file(&path)?;
            tracing::debug!("session deleted: id={}", id);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!("session delete skipped (not found): id={}", id);
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

/// At most this many per-entry diagnostics are emitted for one discovery call;
/// the rest are summarized so a corrupted directory cannot flood the log.
const MAX_DISCOVERY_DIAGNOSTICS: usize = 5;

/// Per-entry failures encountered while enumerating saved sessions.
#[derive(Default)]
struct SkippedEntries {
    reported: usize,
    total: usize,
}

impl SkippedEntries {
    fn note(&mut self, path: &Path, error: &dyn std::fmt::Display) {
        self.total += 1;
        if self.reported < MAX_DISCOVERY_DIAGNOSTICS {
            self.reported += 1;
            tracing::warn!("session discovery skipped {}: {}", path.display(), error);
        }
    }

    fn finish(&self, operation: &str) {
        if self.total > self.reported {
            tracing::warn!(
                "session discovery ({}) skipped {} entries in total",
                operation,
                self.total
            );
        }
    }
}

/// Decode one candidate session file during multi-session discovery.
///
/// Discovery must stay available when an unrelated entry is unsafe, unreadable,
/// invalid or disappears mid-iteration, so those entries are skipped with a
/// bounded diagnostic instead of aborting the whole search. Exact-identifier
/// loading (`load_session_exact`) stays fail-closed.
fn read_discovered_session(path: &Path, skipped: &mut SkippedEntries) -> Option<Session> {
    let json = match read_private_string(path) {
        Ok(json) => json,
        Err(error) => {
            skipped.note(path, &error);
            return None;
        }
    };
    match decode_session_json(&json) {
        Ok(session) => Some(session),
        Err(error) => {
            skipped.note(path, &error);
            None
        }
    }
}

pub fn find_sessions_by_prefix(prefix: &str) -> anyhow::Result<Vec<Session>> {
    if disabled("sessions") {
        return Ok(Vec::new());
    }
    let Some(dir) = existing_session_dir()? else {
        return Ok(Vec::new());
    };
    let lower = prefix.to_lowercase();
    let mut sessions: Vec<Session> = Vec::new();
    let mut skipped = SkippedEntries::default();
    for entry in std::fs::read_dir(&dir)? {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(error) => {
                skipped.note(&dir, &error);
                continue;
            }
        };
        if path.extension().is_some_and(|e| e == "json")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            && let Some(session) = read_discovered_session(&path, &mut skipped)
            && (stem.starts_with(prefix) || session.name.to_lowercase().contains(&lower))
        {
            sessions.push(session);
        }
    }
    skipped.finish("prefix search");
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    sessions.dedup_by(|a, b| a.id == b.id);
    tracing::debug!(
        "find_sessions_by_prefix('{}'): {} results",
        prefix,
        sessions.len(),
    );
    Ok(sessions)
}

/// Load one private session by its complete portable identifier. This exact
/// lookup is also used to reconcile the visible state when an atomic save
/// reports an error after publication may already have occurred.
pub fn load_session_exact(id: &str) -> anyhow::Result<Option<Session>> {
    if disabled("sessions") {
        return Ok(None);
    }
    crate::paths::validate_portable_component(id)?;
    let Some(dir) = existing_session_dir()? else {
        return Ok(None);
    };
    let path = dir.join(format!("{id}.json"));
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            let json = read_private_string(&path)?;
            let session = decode_session_json(&json)
                .map_err(|error| anyhow::anyhow!("invalid persisted session {id}: {error}"))?;
            anyhow::ensure!(
                session.id == id,
                "persisted session ID mismatch: requested {id}, found {}",
                session.id
            );
            Ok(Some(session))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
pub fn find_session_by_name(name: &str) -> anyhow::Result<Option<Session>> {
    if disabled("sessions") {
        return Ok(None);
    }
    let Some(dir) = existing_session_dir()? else {
        return Ok(None);
    };
    let lower = name.to_lowercase();
    let mut skipped = SkippedEntries::default();
    for entry in std::fs::read_dir(&dir)? {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(error) => {
                skipped.note(&dir, &error);
                continue;
            }
        };
        if path.extension().is_some_and(|e| e == "json")
            && let Some(session) = read_discovered_session(&path, &mut skipped)
            && session.name.to_lowercase() == lower
        {
            skipped.finish("name search");
            return Ok(Some(session));
        }
    }
    skipped.finish("name search");
    Ok(None)
}

/// How a `--session <query>` argument resolved against the saved sessions.
pub enum SessionSelection {
    One(Box<Session>),
    NoMatch,
    Ambiguous(Vec<Session>),
}

/// Pick the session a `--session <query>` means from the prefix-search
/// candidates (id prefix or name substring, newest first).
///
/// An exact id wins, then an exact (case-insensitive) name, then a single
/// prefix/substring match. Without the exact-name step a session named `work`
/// could never be selected while another was named `homework`
/// (mini-agent-6wvoz).
pub fn select_session(query: &str, candidates: Vec<Session>) -> SessionSelection {
    if let Some(index) = candidates.iter().position(|s| s.id == query) {
        return SessionSelection::One(Box::new(candidates.into_iter().nth(index).unwrap()));
    }
    let lower = query.to_lowercase();
    let exact_names = candidates
        .iter()
        .filter(|s| !s.name.is_empty() && s.name.to_lowercase() == lower)
        .count();
    if exact_names > 0 {
        let mut named: Vec<Session> = candidates
            .into_iter()
            .filter(|s| !s.name.is_empty() && s.name.to_lowercase() == lower)
            .collect();
        return if named.len() == 1 {
            SessionSelection::One(Box::new(named.remove(0)))
        } else {
            SessionSelection::Ambiguous(named)
        };
    }
    match candidates.len() {
        0 => SessionSelection::NoMatch,
        1 => SessionSelection::One(Box::new(candidates.into_iter().next().unwrap())),
        _ => SessionSelection::Ambiguous(candidates),
    }
}

/// Saved session files, newest modification first.
fn session_paths_newest_first(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|e| e == "json"))
        .map(|e| {
            let path = e.path();
            let mtime = std::fs::symlink_metadata(&path)
                .ok()
                .and_then(|m| m.modified().ok());
            (mtime, path)
        })
        .collect();
    entries.sort_by_key(|b| std::cmp::Reverse(b.0));
    Ok(entries.into_iter().map(|(_, path)| path).collect())
}

#[cfg(test)]
pub fn find_recent_sessions(limit: usize) -> anyhow::Result<Vec<Session>> {
    if disabled("sessions") {
        return Ok(Vec::new());
    }
    let Some(dir) = existing_session_dir()? else {
        return Ok(Vec::new());
    };
    // Sort by filesystem mtime to avoid loading all sessions
    let entries = session_paths_newest_first(&dir)?;

    let mut sessions: Vec<Session> = Vec::new();
    let mut skipped = SkippedEntries::default();
    // The limit applies to sessions that actually decoded: a run of broken
    // recent files must not hide older valid sessions.
    for path in entries.iter() {
        if sessions.len() == limit {
            break;
        }
        if let Some(session) = read_discovered_session(path, &mut skipped) {
            sessions.push(session);
        }
    }
    skipped.finish("recent sessions");
    tracing::debug!(
        "find_recent_sessions(limit={}): {} results",
        limit,
        sessions.len(),
    );
    Ok(sessions)
}

/// Return recent sessions whose persisted workspace is exactly the captured
/// workspace. `--continue` uses this instead of silently importing history and
/// approvals from whichever project happened to run most recently.
///
/// Files are visited newest first, and only a file whose `working_dir`
/// matches is fully decoded, so `--continue` stops at the first match instead
/// of deserializing every saved session (mini-agent-5s9me).
pub fn find_recent_sessions_for_workspace(
    limit: usize,
    workspace: &Path,
) -> anyhow::Result<Vec<Session>> {
    #[derive(serde::Deserialize)]
    struct WorkspacePeek {
        #[serde(default)]
        working_dir: String,
    }

    let workspace = std::fs::canonicalize(workspace)?;
    let mut matches = Vec::new();
    if limit == 0 || disabled("sessions") {
        return Ok(matches);
    }
    let Some(dir) = existing_session_dir()? else {
        return Ok(matches);
    };
    let mut skipped = SkippedEntries::default();
    for path in session_paths_newest_first(&dir)? {
        let json = match read_private_string(&path) {
            Ok(json) => json,
            Err(error) => {
                skipped.note(&path, &error);
                continue;
            }
        };
        let peek = match serde_json::from_str::<WorkspacePeek>(&json) {
            Ok(peek) => peek,
            Err(error) => {
                skipped.note(&path, &error);
                continue;
            }
        };
        let saved = Path::new(peek.working_dir.as_str());
        if std::fs::canonicalize(saved).ok().as_deref() != Some(workspace.as_path()) {
            continue;
        }
        match decode_session_json(&json) {
            Ok(session) => matches.push(session),
            Err(error) => {
                skipped.note(&path, &error);
                continue;
            }
        }
        if matches.len() == limit {
            break;
        }
    }
    skipped.finish("workspace sessions");
    Ok(matches)
}

pub fn agents_path() -> PathBuf {
    app_paths().global_agents_file()
}

#[cfg(feature = "archmd")]
pub fn architecture_path() -> PathBuf {
    app_paths().global_architecture_file()
}

pub fn suffix_path() -> PathBuf {
    app_paths().suffix_file()
}

pub fn load_suffix() -> Option<String> {
    let path = suffix_path();
    if path.exists() {
        std::fs::read_to_string(path)
            .ok()
            .filter(|s| !s.trim().is_empty())
    } else {
        None
    }
}

fn theme_file_path() -> PathBuf {
    app_paths().theme_selection_file()
}

pub fn save_theme_name(name: Option<&str>) -> anyhow::Result<()> {
    let path = theme_file_path();
    if let Some(parent) = path.parent() {
        crate::paths::ensure_private_directory(parent)?;
    }
    let value = match name {
        Some(n) => serde_json::json!({ "theme": n }),
        None => serde_json::json!({ "theme": null }),
    };
    atomic_write(&path, &serde_json::to_string_pretty(&value)?)?;
    Ok(())
}

pub fn load_theme_name() -> Option<String> {
    let path = theme_file_path();
    let content = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    value.get("theme")?.as_str().map(|s| s.to_string())
}

#[cfg(test)]
mod readonly_import_tests {
    use super::*;
    use crate::paths::WorkspaceBinding;

    fn snapshot(
        root: &crate::tests::OwnedTestDirectory,
    ) -> (
        std::sync::Arc<WorkspaceBinding>,
        WorkspaceBinding,
        Session,
        PathBuf,
    ) {
        let state = root.path().join("state");
        crate::paths::ensure_private_directory(&state).unwrap();
        let workspace = std::sync::Arc::new(WorkspaceBinding::capture(root.path()).unwrap());
        let sessions = WorkspaceBinding::capture(&state).unwrap();
        let mut session = Session::new("untrusted-label", "data-model", 32_000, "");
        session.working_dir = workspace.root().to_string_lossy().into();
        session.add_message(crate::session::MessageRole::User, "literal saved history");
        session
            .permission_allowlist
            .push(crate::session::PermissionAllowEntry {
                tool: "write".into(),
                pattern: "**".into(),
            });
        let path = state.join(format!("{}.json", session.id));
        crate::fs::private_atomic_write_sync(
            &path,
            serde_json::to_string(&session).unwrap().as_bytes(),
        )
        .unwrap();
        (workspace, sessions, session, path)
    }

    #[test]
    fn readonly_session_import_is_exact_bounded_and_never_recreates_grants_or_missing_artifacts() {
        let root = crate::tests::OwnedTestDirectory::new("readonly-session-import");
        let (workspace, sessions, mut original, path) = snapshot(&root);
        let before = crate::fs::checked_path_metadata(&path).unwrap();
        let loaded = load_session_exact_read_only(&sessions, &original.id, &workspace)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.id, original.id);
        assert_eq!(loaded.messages[0].content, "literal saved history");
        assert!(loaded.permission_allowlist.is_empty());
        assert!(loaded.rewind_undo.is_none());
        crate::fs::ensure_same_file(
            &path,
            &before,
            &crate::fs::checked_path_metadata(&path).unwrap(),
        )
        .unwrap();
        // The readonly identity proof is complete. Retire its held descriptor
        // before this fixture deliberately replaces the snapshot for negatives.
        drop(before);
        let absent = uuid::Uuid::new_v4().to_string();
        assert!(
            load_session_exact_read_only(&sessions, &absent, &workspace)
                .unwrap()
                .is_none()
        );
        assert!(!sessions.root().join(format!("{absent}.json")).exists());
        assert!(load_session_exact_read_only(&sessions, "../credentials", &workspace).is_err());
        original.id = uuid::Uuid::new_v4().to_string().into();
        crate::fs::private_atomic_write_sync(
            &path,
            serde_json::to_string(&original).unwrap().as_bytes(),
        )
        .unwrap();
        assert!(
            load_session_exact_read_only(&sessions, &loaded.id, &workspace)
                .unwrap_err()
                .to_string()
                .contains("identity mismatch")
        );
        original.id = loaded.id.clone();
        original.working_dir = sessions.root().to_string_lossy().into();
        crate::fs::private_atomic_write_sync(
            &path,
            serde_json::to_string(&original).unwrap().as_bytes(),
        )
        .unwrap();
        assert!(
            load_session_exact_read_only(&sessions, &loaded.id, &workspace)
                .unwrap_err()
                .to_string()
                .contains("another workspace")
        );
        crate::fs::private_atomic_write_sync(&path, &vec![b' '; MAX_SESSION_BYTES + 1]).unwrap();
        assert!(
            load_session_exact_read_only(&sessions, &loaded.id, &workspace)
                .unwrap_err()
                .to_string()
                .contains("8 MiB")
        );
    }

    #[cfg(unix)]
    #[test]
    fn readonly_session_import_refuses_unsafe_file_and_root_without_permission_repair() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
        let root = crate::tests::OwnedTestDirectory::new("readonly-session-policy");
        let (workspace, sessions, original, path) = snapshot(&root);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_session_exact_read_only(&sessions, &original.id, &workspace).is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o644);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let alias = sessions.root().join("hardlink.json");
        std::fs::hard_link(&path, &alias).unwrap();
        assert!(load_session_exact_read_only(&sessions, &original.id, &workspace).is_err());
        std::fs::remove_file(alias).unwrap();
        std::fs::set_permissions(sessions.root(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(load_session_exact_read_only(&sessions, &original.id, &workspace).is_err());
        assert_eq!(
            std::fs::metadata(sessions.root()).unwrap().mode() & 0o777,
            0o755
        );
        std::fs::set_permissions(sessions.root(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_file(&path).unwrap();
        symlink(root.path().join("outside.json"), &path).unwrap();
        assert!(load_session_exact_read_only(&sessions, &original.id, &workspace).is_err());
    }

    #[test]
    fn session_storage_global_limit_refuses_before_creating_and_keeps_errors_static() {
        let root = crate::tests::OwnedTestDirectory::new("session-save-bound");
        let _environment = crate::tests::ScopedProcessEnv::set(&[
            (
                "ZS_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "ZS_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "ZS_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "ZS_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
        ]);
        let mut invalid_projection = Session::new("provider", "model", 32_000, "");
        invalid_projection.add_user_message(
            crate::session::user::PersistedUserMessage::new(vec![
                crate::session::user::UserBlock::Text {
                    text: "canonical typed user text".into(),
                },
            ])
            .unwrap(),
        );
        invalid_projection.messages[0].content = "divergent public display projection".into();
        assert!(save_session(&invalid_projection).is_err());
        assert!(!root.path().join("state").exists());

        let mut session = Session::new("provider", "model", 32_000, "");
        session.add_message(
            crate::session::MessageRole::User,
            &"x".repeat(MAX_SESSION_BYTES),
        );
        assert!(
            serialize_bounded_session(&session)
                .unwrap_err()
                .to_string()
                .contains("8 MiB")
        );
        assert!(
            save_session(&session)
                .unwrap_err()
                .to_string()
                .contains("8 MiB")
        );
        assert!(!root.path().join("state").exists());
        let path = session_path(&session.id).unwrap();
        let previous =
            serde_json::to_string(&Session::new("provider", "model", 32_000, "previous")).unwrap();
        atomic_write(&path, &previous).unwrap();
        assert!(
            save_session(&session)
                .unwrap_err()
                .to_string()
                .contains("8 MiB")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), previous);
        // UTF-8 source size is not the encoded size: escaped content must be
        // checked as it is written, before the existing snapshot is replaced.
        session.messages.clear();
        session.add_message(
            crate::session::MessageRole::User,
            &"\"".repeat(MAX_SESSION_BYTES / 2),
        );
        assert!(session.messages[0].content.len() < MAX_SESSION_BYTES);
        assert!(
            save_session(&session)
                .unwrap_err()
                .to_string()
                .contains("8 MiB")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), previous);
        let oversized = serde_json::to_string(&session).unwrap();
        assert!(
            decode_session_json(&oversized)
                .unwrap_err()
                .to_string()
                .contains("8 MiB")
        );
        assert_eq!(
            decode_session_json("{\"messages\": [\"opaque-secret\"]}")
                .unwrap_err()
                .to_string(),
            "invalid persisted session JSON"
        );
        invalid_projection.id = session.id.clone();
        assert!(save_session(&invalid_projection).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), previous);
        invalid_projection.messages[0].content = "canonical typed user text".into();
        invalid_projection.messages[0].role = crate::session::MessageRole::Assistant;
        assert!(save_session(&invalid_projection).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), previous);
    }
}
